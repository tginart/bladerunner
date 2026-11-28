//! Mission Control parts of the hub browser: the fleet strip, the input reason
//! of a blocked agent, the jump to the next blocked agent, the new-channel
//! form (`c`), the hub switcher (`H`), and the `/hub here` toggle (`h`).
//!
//! These parts only read the hub and return outcomes. The controller writes
//! the hub. Only the person view writes read cursors, as in the rest of the
//! browser.

use std::path::PathBuf;

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;
use xai_grok_tools::implementations::grok_build::agent_hub::{MemberKind, store};

use super::{
    HoverKey, HubBrowserState, HubKeyOutcome, HubSnapshot, Row, contains, fill,
    picker_viewport_top, span_at,
};
use crate::theme::Theme;

/// The status word of an agent that waits for a person.
const NEEDS_YOU: &str = "needs you";
/// The top bar label of the private hub of a session.
const SESSION_HUB_LABEL: &str = "this session (private)";
/// Between two fleet strip segments.
const SEPARATOR: &str = " · ";
/// Columns at the left of the top bar that the `/hub here` badge leaves free.
const MIN_LEFT_COLS: u16 = 24;
/// Widest the new-channel form gets.
const FORM_MAX_WIDTH: u16 = 60;
const DRAFT_BLOCKS_SWITCH: &str = "Send or clear your draft before switching hubs.";

/// Live agents of a hub, counted by what they do now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetCounts {
    pub working: usize,
    pub needs_input: usize,
    pub waiting: usize,
    pub idle: usize,
}

impl FleetCounts {
    fn total(&self) -> usize {
        self.working + self.needs_input + self.waiting + self.idle
    }
}

/// What one fleet strip segment counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FleetKind {
    Working,
    NeedsInput,
    Waiting,
    Idle,
    /// The `0 live` text when no agent is live.
    Live,
}

/// One segment of the fleet strip, such as `2 working`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FleetSegment {
    kind: FleetKind,
    count: usize,
}

impl FleetSegment {
    fn text(&self) -> String {
        let count = self.count;
        match self.kind {
            FleetKind::Working => format!("{count} working"),
            FleetKind::NeedsInput => format!("{count} {NEEDS_YOU}"),
            FleetKind::Waiting => format!("{count} waiting"),
            FleetKind::Idle => format!("{count} idle"),
            FleetKind::Live => format!("{count} live"),
        }
    }
}

/// The new-channel form (`c`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelForm {
    pub name: String,
    pub purpose: String,
    /// True when the keys go to the purpose, false for the name.
    pub is_editing_purpose: bool,
    /// Why the last create failed.
    pub error: Option<String>,
}

impl ChannelForm {
    /// Add one typed character. A channel name is lowercase, with `-` for a space.
    fn push(&mut self, c: char) {
        if self.is_editing_purpose {
            self.purpose.push(c);
        } else if c.is_whitespace() {
            self.name.push('-');
        } else {
            self.name.push(c.to_ascii_lowercase());
        }
    }

    /// Add pasted text to the edited field. The fields are one line.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars() {
            self.push(if c == '\r' || c == '\n' { ' ' } else { c });
        }
    }

    fn pop(&mut self) {
        if self.is_editing_purpose {
            self.purpose.pop();
        } else {
            self.name.pop();
        }
    }
}

/// One hub that the hub switcher lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubChoice {
    /// The `Hub::name`, which `store::Hub::open_by_name` opens.
    pub name: String,
    pub label: String,
}

/// The open hub switcher (`H`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPicker {
    pub choices: Vec<HubChoice>,
    pub highlighted: usize,
}

/// Purpose: Count the live agents of a hub by their presence.
/// Input: The snapshot of the hub.
/// Output: The counts. Persons, hub-bot, and agents that are not live are
///     left out. A live agent with no presence file counts as idle.
pub fn fleet_counts(snapshot: &HubSnapshot) -> FleetCounts {
    let mut counts = FleetCounts::default();
    for member in &snapshot.meta.members {
        if !is_live_agent(member) {
            continue;
        }
        match snapshot.presences.get(&member.handle).map(|p| p.activity) {
            Some(store::Activity::Working) => counts.working += 1,
            Some(store::Activity::NeedsInput) => counts.needs_input += 1,
            Some(store::Activity::Waiting) => counts.waiting += 1,
            Some(store::Activity::Idle) | None => counts.idle += 1,
        }
    }
    counts
}

/// Purpose: Find the live agent that waited longest for a person.
/// Input: The snapshot of the hub.
/// Output: The handle, or None when no live agent needs input. The oldest
///     `since_ms` comes first. The handle breaks a tie.
pub fn next_needs_input_member(snapshot: &HubSnapshot) -> Option<String> {
    snapshot
        .meta
        .members
        .iter()
        .filter(|member| is_live_agent(member))
        .filter_map(|member| {
            let presence = snapshot.presences.get(&member.handle)?;
            let is_blocked = presence.activity == store::Activity::NeedsInput;
            is_blocked.then_some((presence.since_ms, &member.handle))
        })
        .min()
        .map(|(_, handle)| handle.clone())
}

/// The status word of an agent that needs input. Presence files from older
/// binaries have no reason, so they show the plain word.
pub fn needs_input_word(reason: Option<store::InputReason>) -> &'static str {
    match reason {
        Some(store::InputReason::Permission) => "needs you (permission)",
        Some(store::InputReason::Question) => "needs you (question)",
        Some(store::InputReason::PlanApproval) => "needs you (plan)",
        None => NEEDS_YOU,
    }
}

/// Purpose: Keep a sidebar name readable next to a long status word.
/// Input: The right-hand text of a sidebar row, the name width, and the row width.
/// Output: The text. A `needs you (...)` word that leaves no room for the
///     name becomes `needs you`.
pub fn fit_sidebar_word(word: String, name_width: usize, row_width: u16) -> String {
    // The row has 2 columns of margin, a 2-column glyph, and gaps of 1 + 1.
    let is_too_wide = name_width + word.width() + 6 > row_width as usize;
    if is_too_wide && word.starts_with(NEEDS_YOU) {
        return NEEDS_YOU.to_string();
    }
    word
}

/// Purpose: Flip `/hub here` of a hub under the hub lock.
/// Input: The hub.
/// Output: The new `person_posts_alert_all` value, or an error for an
///     archived hub or a failed write.
pub fn toggle_person_alerts(hub: &store::Hub) -> Result<bool> {
    hub.update_meta(|meta| -> Result<bool> {
        if meta.archived {
            bail!("hub {:?} is archived (read-only)", meta.name);
        }
        meta.person_posts_alert_all = !meta.person_posts_alert_all;
        Ok(meta.person_posts_alert_all)
    })?
}

impl HubBrowserState {
    /// The directory that holds every hub, the parent of the shown hub.
    pub fn hubs_root(&self) -> PathBuf {
        self.hub
            .dir()
            .parent()
            .expect("a hub directory is inside the hubs root")
            .to_path_buf()
    }

    /// Purpose: Go to the live agent that waited longest for a person (`n`).
    /// Input: The browser state.
    /// Output: `GoToMember` for that agent, after the sidebar selects its DM
    ///     when the DM is listed. With no such agent, a notice.
    pub(super) fn jump_to_needs_input(&mut self) -> HubKeyOutcome {
        let Some(handle) = next_needs_input_member(&self.snapshot) else {
            self.notice = Some("No members need input.".to_string());
            return HubKeyOutcome::Changed;
        };
        let row = Row::Dm {
            peer: handle.clone(),
            conv: store::dm_conv(&self.perspective, &handle),
        };
        if self.rows().contains(&row) {
            self.stop_compose();
            self.select(Some(row));
        }
        HubKeyOutcome::GoToMember(handle)
    }

    /// Ask the controller to flip `/hub here` (`h`). An archived hub gets a notice.
    pub(super) fn request_toggle_here(&mut self) -> HubKeyOutcome {
        if self.reject_archived() {
            return HubKeyOutcome::Changed;
        }
        HubKeyOutcome::ToggleHere
    }

    /// Purpose: Show the result of a `/hub here` toggle.
    /// Input: The new setting, or the error.
    /// Output: None. The hub is read again, and a notice states the setting.
    pub fn finish_toggle_here(&mut self, result: Result<bool>) {
        if let Err(error) = self.reload() {
            tracing::warn!(%error, "hub browser: reload failed");
        }
        self.notice = Some(match result {
            Ok(true) => "/hub here on: your channel posts wake every member.".to_string(),
            Ok(false) => {
                "/hub here off: your channel posts wake only the @handles they name.".to_string()
            }
            Err(error) => format!("Could not change /hub here: {error}"),
        });
    }

    /// Open the new-channel form (`c`). An archived hub gets a notice.
    pub(super) fn open_channel_form(&mut self) {
        if !self.reject_archived() {
            self.channel_form = Some(ChannelForm::default());
        }
    }

    /// Purpose: Edit or submit the new-channel form.
    /// Input: A key event while the form is open.
    /// Output: `CreateChannel` on Enter with a name. Esc closes the form.
    pub(super) fn handle_channel_form_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        let Some(form) = self.channel_form.as_mut() else {
            return HubKeyOutcome::Changed;
        };
        match key.code {
            KeyCode::Esc => self.channel_form = None,
            KeyCode::Tab | KeyCode::BackTab => form.is_editing_purpose = !form.is_editing_purpose,
            KeyCode::Enter if form.name.is_empty() => {
                form.error = Some("Type a channel name.".to_string());
            }
            KeyCode::Enter => {
                return HubKeyOutcome::CreateChannel {
                    name: form.name.clone(),
                    purpose: form.purpose.clone(),
                };
            }
            KeyCode::Backspace => form.pop(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                form.push(c);
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    /// Purpose: Show the result of a channel create.
    /// Input: The normalized channel name, or the error.
    /// Output: None. On success the form closes and the sidebar selects the
    ///     new channel. On error the form stays open and shows why.
    pub fn finish_create_channel(&mut self, result: Result<String>) {
        let name = match result {
            Ok(name) => name,
            Err(error) => {
                if let Some(form) = self.channel_form.as_mut() {
                    form.error = Some(error.to_string());
                }
                return;
            }
        };
        self.channel_form = None;
        if let Err(error) = self.reload() {
            tracing::warn!(%error, "hub browser: reload failed");
        }
        self.select(Some(Row::Channel(name.clone())));
        self.notice = Some(format!("Created #{name}."));
    }

    /// Purpose: Open the hub switcher (`H`).
    /// Input: The browser state and the hubs on disk.
    /// Output: None. A draft or a list error gives a notice instead, because
    ///     a switch would lose the draft.
    pub(super) fn open_hub_picker(&mut self) {
        if !self.composer.is_empty() {
            self.notice = Some(DRAFT_BLOCKS_SWITCH.to_string());
            return;
        }
        let metas = match store::list_hubs(&self.hubs_root(), false) {
            Ok(metas) => metas,
            Err(error) => {
                self.notice = Some(format!("Could not list hubs: {error}"));
                return;
            }
        };
        let choices = hub_choices(&metas, self.session_hub.as_deref());
        if choices.is_empty() {
            self.notice = Some("No hubs to show.".to_string());
            return;
        }
        let highlighted = choices
            .iter()
            .position(|choice| choice.name == self.hub.name())
            .unwrap_or(0);
        self.hub_picker = Some(HubPicker {
            choices,
            highlighted,
        });
    }

    pub(super) fn handle_hub_picker_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        let Some(picker) = self.hub_picker.as_mut() else {
            return HubKeyOutcome::Changed;
        };
        let last = picker.choices.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('H') | KeyCode::Char('q') => self.hub_picker = None,
            KeyCode::Up | KeyCode::Char('k') => {
                picker.highlighted = picker.highlighted.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                picker.highlighted = (picker.highlighted + 1).min(last);
            }
            KeyCode::Enter => {
                let idx = picker.highlighted;
                return self.choose_hub(idx);
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    /// Close the hub switcher. Return `SwitchHub` for a hub that is not shown now.
    fn choose_hub(&mut self, idx: usize) -> HubKeyOutcome {
        let Some(picker) = self.hub_picker.take() else {
            return HubKeyOutcome::Changed;
        };
        match picker.choices.into_iter().nth(idx) {
            Some(choice) if choice.name != self.hub.name() => HubKeyOutcome::SwitchHub(choice.name),
            _ => HubKeyOutcome::Changed,
        }
    }

    /// Purpose: Show another hub in place, as `/hub open <name>` does.
    /// Input: The hub to show.
    /// Output: Ok, or the read error. The person name and the post history
    ///     stay. A nonempty draft blocks the switch, so it is never lost.
    /// Note: The switch only views the hub. The session stays in its own hub.
    pub fn switch_hub(&mut self, hub: store::Hub) -> Result<()> {
        if !self.composer.is_empty() {
            bail!("{DRAFT_BLOCKS_SWITCH}");
        }
        let mut next = Self::open(hub, &self.person_name, None)?;
        next.composer.set_history(self.composer.history().to_vec());
        next.session_hub = self.session_hub.clone();
        next.notice = Some(format!("Showing {}.", hub_label(&next.snapshot.meta)));
        *self = next;
        Ok(())
    }

    /// Give a notice and return true when the shown hub is archived.
    fn reject_archived(&mut self) -> bool {
        let meta = &self.snapshot.meta;
        if !meta.archived {
            return false;
        }
        self.notice = Some(format!("Hub {} is archived (read-only).", meta.name));
        true
    }

    /// Purpose: Follow a click on a top bar control.
    /// Input: The screen cell.
    /// Output: The outcome, or None when the cell has no top bar control.
    pub(super) fn click_top_bar(&mut self, col: u16, row: u16) -> Option<HubKeyOutcome> {
        let hits = &self.hits;
        let outcome = if contains(hits.hub_chip, col, row) {
            self.open_hub_picker();
            HubKeyOutcome::Changed
        } else if contains(hits.needs_chip, col, row) {
            self.jump_to_needs_input()
        } else if contains(hits.here_chip, col, row) {
            self.request_toggle_here()
        } else {
            return None;
        };
        self.pending_stop = None;
        Some(outcome)
    }

    pub(super) fn top_bar_hover_at(&self, col: u16, row: u16) -> Option<HoverKey> {
        let hits = &self.hits;
        if contains(hits.hub_chip, col, row) {
            Some(HoverKey::HubChip)
        } else if contains(hits.needs_chip, col, row) {
            Some(HoverKey::NeedsChip)
        } else if contains(hits.here_chip, col, row) {
            Some(HoverKey::HereChip)
        } else {
            None
        }
    }

    /// Purpose: Give mouse events to the open hub switcher or channel form.
    /// Input: The event kind and the screen cell.
    /// Output: The outcome, or None when neither is open. A click outside
    ///     closes it. A footer hint click presses its key.
    pub(super) fn handle_mission_overlay_mouse(
        &mut self,
        kind: MouseEventKind,
        col: u16,
        row: u16,
    ) -> Option<HubKeyOutcome> {
        if self.hub_picker.is_none() && self.channel_form.is_none() {
            return None;
        }
        let outcome = match kind {
            MouseEventKind::Moved => self.hover_hub_picker(col, row),
            MouseEventKind::Down(MouseButton::Left) => self.click_mission_overlay(col, row),
            _ => HubKeyOutcome::Unchanged,
        };
        Some(outcome)
    }

    fn hover_hub_picker(&mut self, col: u16, row: u16) -> HubKeyOutcome {
        let hit = self.hub_picker_row_at(col, row);
        let Some((picker, idx)) = self.hub_picker.as_mut().zip(hit) else {
            return HubKeyOutcome::Unchanged;
        };
        if picker.highlighted == idx {
            return HubKeyOutcome::Unchanged;
        }
        picker.highlighted = idx;
        HubKeyOutcome::Changed
    }

    fn click_mission_overlay(&mut self, col: u16, row: u16) -> HubKeyOutcome {
        let hint_key = self
            .hits
            .footer_hints
            .iter()
            .find(|(rect, _)| contains(*rect, col, row))
            .and_then(|(_, key)| *key);
        if let Some(key) = hint_key {
            return self.handle_key(&key.to_key_event());
        }
        if self.hub_picker.is_some() {
            return match self.hub_picker_row_at(col, row) {
                Some(idx) => self.choose_hub(idx),
                None => {
                    self.hub_picker = None;
                    HubKeyOutcome::Changed
                }
            };
        }
        if !contains(self.hits.channel_form, col, row) {
            self.channel_form = None;
        }
        HubKeyOutcome::Changed
    }

    fn hub_picker_row_at(&self, col: u16, row: u16) -> Option<usize> {
        self.hits
            .hub_picker
            .iter()
            .find(|(rect, _)| contains(*rect, col, row))
            .map(|(_, idx)| *idx)
    }
}

/// Purpose: List the hubs that the hub switcher shows.
/// Input: The visible named hubs and the `Hub::name` of the session hub, if any.
/// Output: The session hub first, then the named hubs in name order.
fn hub_choices(metas: &[store::HubMeta], session_hub: Option<&str>) -> Vec<HubChoice> {
    let mut choices = Vec::new();
    if let Some(name) = session_hub {
        choices.push(HubChoice {
            name: name.to_string(),
            label: SESSION_HUB_LABEL.to_string(),
        });
    }
    for meta in metas {
        choices.push(HubChoice {
            name: meta.name.clone(),
            label: meta.name.clone(),
        });
    }
    choices
}

/// The name that the browser shows for a hub.
fn hub_label(meta: &store::HubMeta) -> &str {
    if meta.is_session_hub() {
        SESSION_HUB_LABEL
    } else {
        &meta.name
    }
}

fn is_live_agent(member: &store::Member) -> bool {
    member.kind == MemberKind::Agent && member.active && member.handle != store::BOT_HANDLE
}

/// Purpose: Choose the fleet strip segments that fit a width.
/// Input: The fleet counts and the widest the strip can be, in columns.
/// Output: The segments in the order working, needs you, waiting, idle.
/// Method:
///     1. With no live agent, return `0 live`.
///     2. Start with all four segments.
///     3. While the strip is too wide, drop the next segment of the drop
///        order: a zero idle, a zero waiting, then idle, then waiting.
/// Note: `N needs you` and `N working` always stay. The caller clips them.
fn fleet_segments(counts: FleetCounts, max_width: usize) -> Vec<FleetSegment> {
    if counts.total() == 0 {
        return vec![FleetSegment {
            kind: FleetKind::Live,
            count: 0,
        }];
    }
    let mut segments = vec![
        FleetSegment {
            kind: FleetKind::Working,
            count: counts.working,
        },
        FleetSegment {
            kind: FleetKind::NeedsInput,
            count: counts.needs_input,
        },
        FleetSegment {
            kind: FleetKind::Waiting,
            count: counts.waiting,
        },
        FleetSegment {
            kind: FleetKind::Idle,
            count: counts.idle,
        },
    ];
    let drop_order = [
        (FleetKind::Idle, true),
        (FleetKind::Waiting, true),
        (FleetKind::Idle, false),
        (FleetKind::Waiting, false),
    ];
    for (kind, is_zero_only) in drop_order {
        if strip_width(&segments) <= max_width {
            break;
        }
        segments.retain(|s| s.kind != kind || (is_zero_only && s.count > 0));
    }
    segments
}

fn strip_width(segments: &[FleetSegment]) -> usize {
    let text: usize = segments.iter().map(|s| s.text().width()).sum();
    text + SEPARATOR.width() * segments.len().saturating_sub(1)
}

/// Purpose: Draw the `/hub here` badge left of the perspective chip.
/// Input: The buffer, the bar, the x where the perspective chip label
///     starts, the state, and the theme.
/// Output: The x where the right-hand controls start. A narrow bar has no badge.
pub(super) fn render_here_badge(
    buf: &mut Buffer,
    bar: Rect,
    right_x: u16,
    state: &mut HubBrowserState,
    theme: &Theme,
) -> u16 {
    let meta = &state.snapshot.meta;
    let label = if meta.person_posts_alert_all {
        "[here: on]"
    } else {
        "[here: off]"
    };
    let width = label.width() as u16;
    let x = right_x.saturating_sub(width + 2);
    if x < bar.x + MIN_LEFT_COLS {
        return right_x;
    }
    let fg = if meta.archived {
        theme.gray_dim
    } else if meta.person_posts_alert_all {
        theme.accent_success
    } else {
        theme.gray
    };
    let mut style = Style::default().fg(fg).bg(theme.bg_dark);
    if state.hover == Some(HoverKey::HereChip) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    span_at(buf, x, bar.y, label, style, right_x);
    state.hits.here_chip = Rect::new(x, bar.y, width, 1);
    x
}

/// Purpose: Draw the hub chip at the left of the top bar.
/// Input: The buffer, the bar, the right limit, the state, and the theme.
/// Output: The x after the chip. The chip opens the hub switcher.
pub(super) fn render_hub_chip(
    buf: &mut Buffer,
    bar: Rect,
    max_x: u16,
    state: &mut HubBrowserState,
    theme: &Theme,
) -> u16 {
    let bar_bg = theme.bg_dark;
    let start = bar.x + 1;
    let glyph_style = Style::default().fg(theme.accent_verify).bg(bar_bg);
    let mut x = span_at(buf, start, bar.y, "◆ ", glyph_style, max_x);
    let mut name_style = Style::default()
        .fg(theme.text_primary)
        .bg(bar_bg)
        .add_modifier(Modifier::BOLD);
    if state.hover == Some(HoverKey::HubChip) {
        name_style = name_style.add_modifier(Modifier::UNDERLINED);
    }
    let label = hub_label(&state.snapshot.meta).to_string();
    x = span_at(buf, x, bar.y, &label, name_style, max_x);
    x = span_at(buf, x, bar.y, " ▾", theme.muted().bg(bar_bg), max_x);
    state.hits.hub_chip = Rect::new(start, bar.y, x.saturating_sub(start), 1);
    x
}

/// Purpose: Draw the fleet strip, such as `1 working · 1 needs you · 2 idle`.
/// Input: The buffer, the start cell, the right limit, the state, and the theme.
/// Output: The x after the strip. The `needs you` segment becomes a mouse target.
pub(super) fn render_fleet_strip(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    max_x: u16,
    state: &mut HubBrowserState,
    theme: &Theme,
) -> u16 {
    if max_x <= x {
        return x;
    }
    let bar_bg = theme.bg_dark;
    let segments = fleet_segments(fleet_counts(&state.snapshot), (max_x - x) as usize);
    let mut x = x;
    for (idx, segment) in segments.iter().enumerate() {
        if idx > 0 {
            x = span_at(buf, x, y, SEPARATOR, theme.muted().bg(bar_bg), max_x);
        }
        let fg = match segment.kind {
            FleetKind::Working => theme.running,
            FleetKind::NeedsInput => theme.accent_error,
            FleetKind::Waiting => theme.accent_verify,
            FleetKind::Idle => theme.accent_success,
            FleetKind::Live => theme.gray,
        };
        let mut style = Style::default().fg(fg).bg(bar_bg);
        let is_needs = segment.kind == FleetKind::NeedsInput;
        if is_needs && segment.count > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if is_needs && state.hover == Some(HoverKey::NeedsChip) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        let start = x;
        x = span_at(buf, x, y, &segment.text(), style, max_x);
        if is_needs {
            state.hits.needs_chip = Rect::new(start, y, x - start, 1);
        }
    }
    x
}

/// Purpose: Draw the hub switcher under the hub chip.
/// Input: The buffer, the browser area, the state, and the theme.
/// Output: None. Each hub row becomes a mouse target. `✓` marks the shown hub.
pub(super) fn render_hub_picker(
    buf: &mut Buffer,
    area: Rect,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let Some(picker) = state.hub_picker.as_ref() else {
        return;
    };
    let content_w = picker
        .choices
        .iter()
        .map(|choice| choice.label.width() + 6)
        .max()
        .unwrap_or(10)
        .max(28) as u16;
    let width = (content_w + 2).min(area.width);
    let title_rows = 2;
    let count = picker.choices.len();
    let height = (count as u16 + title_rows + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: state.hits.hub_chip.x.max(area.x),
        y: area.y + 1,
        width,
        height,
    };
    draw_box(buf, rect, theme);
    let inner_x = rect.x + 2;
    let inner_right = rect.right().saturating_sub(2);
    let title_style = theme.muted().bg(theme.bg_base);
    span_at(
        buf,
        inner_x,
        rect.y + 1,
        "Show hub",
        title_style,
        inner_right,
    );
    let visible = rect.height.saturating_sub(title_rows + 2) as usize;
    let first = picker_viewport_top(0, picker.highlighted, visible, count);
    let mut rows = Vec::new();
    for (offset, idx) in (first..count).take(visible).enumerate() {
        let choice = &picker.choices[idx];
        let y = rect.y + 1 + title_rows + offset as u16;
        let is_highlighted = idx == picker.highlighted;
        let bg = if is_highlighted {
            theme.bg_highlight
        } else {
            theme.bg_base
        };
        let row_rect = Rect::new(rect.x + 1, y, rect.width.saturating_sub(2), 1);
        fill(buf, row_rect, Style::default().bg(bg));
        let check = if choice.name == state.hub.name() {
            "✓ "
        } else {
            "  "
        };
        let check_style = Style::default().fg(theme.accent_success).bg(bg);
        let x = span_at(buf, inner_x, y, check, check_style, inner_right);
        let mut name_style = Style::default().fg(theme.text_primary).bg(bg);
        if is_highlighted {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        span_at(buf, x, y, &choice.label, name_style, inner_right);
        rows.push((row_rect, idx));
    }
    state.hits.hub_picker = rows;
}

/// Purpose: Draw the new-channel form in the middle of the browser.
/// Input: The buffer, the browser area, the state, and the theme.
/// Output: None. The terminal cursor goes to the end of the edited field.
pub(super) fn render_channel_form(
    buf: &mut Buffer,
    area: Rect,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let Some(form) = state.channel_form.as_ref() else {
        return;
    };
    let width = FORM_MAX_WIDTH.min(area.width);
    let height = 7.min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    draw_box(buf, rect, theme);
    let bg = theme.bg_base;
    let inner_x = rect.x + 2;
    let inner_right = rect.right().saturating_sub(2);
    let title_style = Style::default()
        .fg(theme.text_primary)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    span_at(
        buf,
        inner_x,
        rect.y + 1,
        "New channel",
        title_style,
        inner_right,
    );
    let label_style = Style::default().fg(theme.gray_bright).bg(bg);
    let text_style = Style::default().fg(theme.text_primary).bg(bg);
    let mut cursor = None;
    for (offset, label, text, is_edited) in [
        (
            2,
            "Name     #",
            form.name.as_str(),
            !form.is_editing_purpose,
        ),
        (
            3,
            "Purpose  ",
            form.purpose.as_str(),
            form.is_editing_purpose,
        ),
    ] {
        let y = rect.y + offset;
        let x = span_at(buf, inner_x, y, label, label_style, inner_right);
        let end = if text.is_empty() && !is_edited {
            span_at(buf, x, y, "optional", theme.muted().bg(bg), inner_right);
            x
        } else {
            span_at(buf, x, y, text, text_style, inner_right)
        };
        if is_edited {
            cursor = Some((end.min(inner_right.saturating_sub(1)), y));
        }
    }
    let (status, status_style) = match &form.error {
        Some(error) => (
            error.as_str(),
            Style::default().fg(theme.accent_error).bg(bg),
        ),
        None => (
            "Enter create · Tab name / purpose · Esc cancel",
            theme.muted().bg(bg),
        ),
    };
    span_at(buf, inner_x, rect.y + 5, status, status_style, inner_right);
    state.cursor = cursor;
    state.hits.channel_form = rect;
}

/// Clear `rect` and draw a rounded border around it.
fn draw_box(buf: &mut Buffer, rect: Rect, theme: &Theme) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    Clear.render(rect, buf);
    fill(buf, rect, Style::default().bg(theme.bg_base));
    let border = Style::default()
        .fg(theme.prompt_border_active)
        .bg(theme.bg_base);
    let inner_w = rect.width.saturating_sub(2) as usize;
    let top = format!("╭{}╮", "─".repeat(inner_w));
    span_at(buf, rect.x, rect.y, &top, border, rect.right());
    for y in rect.y + 1..rect.bottom() - 1 {
        span_at(buf, rect.x, y, "│", border, rect.right());
        span_at(buf, rect.right() - 1, y, "│", border, rect.right());
    }
    let bottom = format!("╰{}╯", "─".repeat(inner_w));
    span_at(
        buf,
        rect.x,
        rect.bottom() - 1,
        &bottom,
        border,
        rect.right(),
    );
}

#[cfg(test)]
mod tests {
    use super::super::render_hub_browser;
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn member(handle: &str, kind: MemberKind, active: bool) -> store::Member {
        store::Member {
            handle: handle.to_string(),
            label: "main".to_string(),
            kind,
            session_id: Some(format!("sid-{handle}")),
            active,
            disconnected: false,
            joined_ms: 0,
            pid: None,
        }
    }

    fn presence(
        activity: store::Activity,
        since_ms: u64,
        input_reason: Option<store::InputReason>,
    ) -> store::Presence {
        store::Presence {
            activity,
            since_ms,
            task: None,
            input_reason,
        }
    }

    /// A hub with one agent per activity, a gone agent, and the person.
    fn fleet_hub() -> (tempfile::TempDir, store::Hub) {
        let root = tempfile::tempdir().unwrap();
        let hub = store::Hub::create(root.path(), "demo").unwrap();
        hub.update_meta(|meta| {
            for handle in ["api", "web", "db", "docs"] {
                meta.members.push(member(handle, MemberKind::Agent, true));
            }
            meta.members.push(member("old", MemberKind::Agent, false));
            meta.members.push(member("me", MemberKind::Person, true));
        })
        .unwrap();
        let now = store::now_ms();
        let reason = Some(store::InputReason::Permission);
        for (handle, activity, reason) in [
            ("api", store::Activity::Working, None),
            ("web", store::Activity::NeedsInput, reason),
            ("db", store::Activity::Waiting, None),
            ("docs", store::Activity::Idle, None),
            // A gone agent and a person never count, whatever their file says.
            ("old", store::Activity::Working, None),
            ("me", store::Activity::Working, None),
        ] {
            hub.save_presence(handle, &presence(activity, now, reason))
                .unwrap();
        }
        (root, hub)
    }

    fn open(hub: store::Hub) -> HubBrowserState {
        HubBrowserState::open(hub, "me", None).unwrap()
    }

    fn render_text(state: &mut HubBrowserState, width: u16) -> String {
        let area = Rect::new(0, 0, width, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, state);
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }
        out
    }

    fn top_line(text: &str) -> &str {
        text.lines().next().unwrap()
    }

    fn submit_channel(state: &mut HubBrowserState, typed: &str) -> HubKeyOutcome {
        state.handle_key(&key(KeyCode::Char('c')));
        for c in typed.chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        state.handle_key(&key(KeyCode::Enter))
    }

    #[test]
    fn the_fleet_strip_counts_only_live_agents() {
        let (_root, hub) = fleet_hub();
        let mut state = open(hub);
        let counts = fleet_counts(&state.snapshot);
        let expected = FleetCounts {
            working: 1,
            needs_input: 1,
            waiting: 1,
            idle: 1,
        };
        assert_eq!(counts, expected);
        let text = render_text(&mut state, 160);
        let top = top_line(&text);
        assert!(
            top.contains("1 working · 1 needs you · 1 waiting · 1 idle"),
            "{top}"
        );
        assert!(top.contains("[here: on]"), "{top}");
        assert!(top.contains("◆ demo ▾"), "{top}");
    }

    #[test]
    fn an_empty_fleet_shows_zero_live() {
        let root = tempfile::tempdir().unwrap();
        let mut state = open(store::Hub::create(root.path(), "demo").unwrap());
        assert!(top_line(&render_text(&mut state, 140)).contains("0 live"));
    }

    #[test]
    fn a_narrow_strip_drops_zero_counts_first_and_keeps_needs_you_and_working() {
        let texts = |counts, width| -> Vec<String> {
            fleet_segments(counts, width)
                .iter()
                .map(FleetSegment::text)
                .collect()
        };
        let counts = FleetCounts {
            working: 2,
            needs_input: 0,
            waiting: 0,
            idle: 3,
        };
        assert_eq!(
            texts(counts, 100),
            ["2 working", "0 needs you", "0 waiting", "3 idle"]
        );
        assert_eq!(texts(counts, 35), ["2 working", "0 needs you", "3 idle"]);
        assert_eq!(texts(counts, 10), ["2 working", "0 needs you"]);
        let busy = FleetCounts {
            working: 1,
            needs_input: 1,
            waiting: 1,
            idle: 1,
        };
        assert_eq!(
            texts(busy, 35),
            ["1 working", "1 needs you", "1 waiting"],
            "with no zero count, idle goes first"
        );
    }

    #[test]
    fn needs_input_words_name_the_reason_and_old_files_get_the_plain_word() {
        let word = |reason| needs_input_word(Some(reason));
        assert_eq!(
            word(store::InputReason::Permission),
            "needs you (permission)"
        );
        assert_eq!(word(store::InputReason::Question), "needs you (question)");
        assert_eq!(word(store::InputReason::PlanApproval), "needs you (plan)");
        assert_eq!(needs_input_word(None), "needs you");
        let fit = |word: &str, name_width| fit_sidebar_word(word.to_string(), name_width, 30);
        assert_eq!(fit("needs you (plan)", 4), "needs you (plan)");
        assert_eq!(fit("needs you (permission)", 8), "needs you");
        assert_eq!(fit("idle 3m", 40), "idle 3m");
    }

    #[test]
    fn the_dm_header_shows_why_an_agent_needs_you() {
        let (_root, hub) = fleet_hub();
        let mut state = open(hub);
        state.open_dm_with("web");
        let text = render_text(&mut state, 140);
        assert!(text.contains("main · needs you (permission)"), "{text}");
        assert!(text.contains("next needs-you"), "footer hint: {text}");
    }

    #[test]
    fn n_goes_to_the_agent_that_waited_longest() {
        let (_root, hub) = fleet_hub();
        let early = presence(store::Activity::NeedsInput, 5, None);
        hub.save_presence("db", &early).unwrap();
        let mut state = open(hub);
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('n'))),
            HubKeyOutcome::GoToMember("db".to_string())
        );
        let row = Row::Dm {
            peer: "db".to_string(),
            conv: store::dm_conv("me", "db"),
        };
        assert_eq!(state.selected_row(), Some(row));
    }

    #[test]
    fn n_with_no_blocked_agent_keeps_the_browser_with_a_notice() {
        let (_root, hub) = fleet_hub();
        let idle = presence(store::Activity::Idle, 0, None);
        hub.save_presence("web", &idle).unwrap();
        let mut state = open(hub);
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('n'))),
            HubKeyOutcome::Changed
        );
        assert_eq!(state.notice.as_deref(), Some("No members need input."));
        let text = render_text(&mut state, 140);
        assert!(!text.contains("next needs-you"), "{text}");
    }

    #[test]
    fn top_bar_chips_jump_toggle_here_and_open_the_switcher() {
        let (_root, hub) = fleet_hub();
        let mut state = open(hub);
        render_text(&mut state, 160);
        let down = MouseEventKind::Down(MouseButton::Left);
        let needs = state.hits.needs_chip;
        assert_eq!(
            state.handle_mouse(down, needs.x, needs.y),
            HubKeyOutcome::GoToMember("web".to_string())
        );
        let here = state.hits.here_chip;
        assert_eq!(
            state.handle_mouse(down, here.x, here.y),
            HubKeyOutcome::ToggleHere
        );
        let chip = state.hits.hub_chip;
        state.handle_mouse(down, chip.x, chip.y);
        assert!(state.hub_picker.is_some());
    }

    #[test]
    fn c_types_a_normalized_channel_and_selects_it_after_the_create() {
        let (_root, hub) = fleet_hub();
        let mut state = open(hub);
        assert_eq!(
            submit_channel(&mut state, ""),
            HubKeyOutcome::Changed,
            "an empty name is not sent"
        );
        for c in "Auth Fix".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("login\nwork");
        let outcome = state.handle_key(&key(KeyCode::Enter));
        let HubKeyOutcome::CreateChannel { name, purpose } = outcome else {
            panic!("expected a create, got {outcome:?}");
        };
        assert_eq!(
            (name.as_str(), purpose.as_str()),
            ("auth-fix", "login work")
        );
        let result = state.hub.create_channel(&name, &purpose);
        state.finish_create_channel(result);
        assert!(state.channel_form.is_none());
        assert_eq!(state.selected_row(), Some(Row::Channel("auth-fix".into())));

        let HubKeyOutcome::CreateChannel { name, purpose } = submit_channel(&mut state, "auth-fix")
        else {
            panic!("expected a create");
        };
        let result = state.hub.create_channel(&name, &purpose);
        state.finish_create_channel(result);
        let form = state
            .channel_form
            .as_ref()
            .expect("a failed create keeps the form");
        assert!(form.error.as_ref().unwrap().contains("already exists"));
        state.handle_key(&key(KeyCode::Esc));
        assert!(state.channel_form.is_none());
    }

    #[test]
    fn h_flips_here_and_persists_it() {
        let (_root, hub) = fleet_hub();
        let mut state = open(hub);
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('h'))),
            HubKeyOutcome::ToggleHere
        );
        let result = toggle_person_alerts(&state.hub);
        state.finish_toggle_here(result);
        assert!(!state.hub.meta().unwrap().person_posts_alert_all);
        assert!(state.notice.as_ref().unwrap().starts_with("/hub here off"));
        assert!(top_line(&render_text(&mut state, 160)).contains("[here: off]"));
        let result = toggle_person_alerts(&state.hub);
        state.finish_toggle_here(result);
        assert!(state.hub.meta().unwrap().person_posts_alert_all);
    }

    #[test]
    fn an_archived_hub_rejects_c_and_h_but_not_h_switch() {
        let (_root, hub) = fleet_hub();
        hub.update_meta(|meta| meta.archived = true).unwrap();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('c')));
        assert!(state.channel_form.is_none());
        assert!(state.notice.as_ref().unwrap().contains("archived"));
        state.notice = None;
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('h'))),
            HubKeyOutcome::Changed
        );
        assert!(state.notice.as_ref().unwrap().contains("archived"));
        assert!(toggle_person_alerts(&state.hub).is_err());
        assert!(state.hub.meta().unwrap().person_posts_alert_all);
        state.session_hub = Some("other".to_string());
        state.handle_key(&key(KeyCode::Char('H')));
        assert!(state.hub_picker.is_some(), "H still switches away");
    }

    #[test]
    fn the_hub_switcher_lists_the_session_hub_and_the_named_hubs() {
        let (root, hub) = fleet_hub();
        store::Hub::create(root.path(), "api-rewrite").unwrap();
        let session_hub = store::Hub::open_session_hub(root.path(), "s1").unwrap();
        let mut state = open(hub);
        state.session_hub = Some(session_hub.name().to_string());
        state.handle_key(&key(KeyCode::Char('H')));
        let picker = state.hub_picker.as_ref().unwrap();
        let labels: Vec<&str> = picker.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["this session (private)", "api-rewrite", "demo"]);
        assert_eq!(picker.highlighted, 2, "the shown hub is highlighted");
        let text = render_text(&mut state, 140);
        assert!(text.contains("✓ demo"), "{text}");
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Changed,
            "the shown hub needs no switch"
        );
        state.handle_key(&key(KeyCode::Char('H')));
        state.handle_key(&key(KeyCode::Up));
        state.handle_key(&key(KeyCode::Up));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::SwitchHub(session_hub.name().to_string())
        );
        assert!(state.hub_picker.is_none());
    }

    #[test]
    fn switch_hub_keeps_the_person_and_history_and_a_draft_blocks_it() {
        let (root, hub) = fleet_hub();
        let other = store::Hub::create(root.path(), "api-rewrite").unwrap();
        let mut state = open(hub);
        state.set_perspective("api").unwrap();
        state.composer.set_history(vec!["hi".to_string()]);
        state.switch_hub(other.clone()).unwrap();
        assert_eq!(state.hub.name(), "api-rewrite");
        assert_eq!(state.person_name(), "me");
        assert!(state.is_person_view());
        assert_eq!(state.composer.history(), ["hi".to_string()]);
        assert_eq!(state.notice.as_deref(), Some("Showing api-rewrite."));

        state.composer.set_text("draft");
        state.handle_key(&key(KeyCode::Char('H')));
        assert!(state.hub_picker.is_none());
        assert_eq!(state.notice.as_deref(), Some(DRAFT_BLOCKS_SWITCH));
        assert!(state.switch_hub(other).is_err());
        assert_eq!(state.composer.text(), "draft");
    }

    #[test]
    fn an_agent_perspective_stays_read_only_through_c_h_and_n() {
        let (_root, hub) = fleet_hub();
        let mut state = open(hub);
        state.set_perspective("api").unwrap();
        let HubKeyOutcome::CreateChannel { name, purpose } = submit_channel(&mut state, "ops")
        else {
            panic!("expected a create");
        };
        let result = state.hub.create_channel(&name, &purpose);
        state.finish_create_channel(result);
        let result = toggle_person_alerts(&state.hub);
        state.finish_toggle_here(result);
        state.handle_key(&key(KeyCode::Char('n')));
        assert_eq!(state.perspective, "api");
        assert!(
            state.hub.cursors("api").unwrap().read.is_empty(),
            "the view never writes the read cursor of an agent"
        );
    }
}
