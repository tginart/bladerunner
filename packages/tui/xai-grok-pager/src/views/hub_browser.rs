//! Hub browser: a full-screen view of one agent hub (`/hub open`).
//!
//! The left pane is a tree: channels, then members. A member expands to the
//! DMs that it is in, so only DMs that have messages show. The right pane is
//! the feed of the selected row, with a post box at the bottom. Reads never
//! move a member's read cursor. A post is the only write, as the person.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;
use xai_grok_tools::implementations::grok_build::agent_hub::{MemberKind, store};

use crate::render::SafeBuf;
use crate::theme::Theme;
use crate::views::modal_window::{
    ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut, render_modal_window,
};

/// Messages that one feed shows.
const FEED_LIMIT: usize = 300;
/// Widest the tree pane gets.
const TREE_MAX_WIDTH: u16 = 36;
/// Rows of the post box under the feed.
const COMPOSE_ROWS: u16 = 3;

/// One row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Section(&'static str),
    Channel(String),
    Member(String),
    /// A DM that `owner` is in, shown under `owner`.
    Dm {
        owner: String,
        peer: String,
        conv: String,
    },
}

impl Row {
    fn is_selectable(&self) -> bool {
        !matches!(self, Row::Section(_))
    }
}

/// Hub data read from disk for one frame of the browser.
#[derive(Debug, Clone)]
pub struct HubSnapshot {
    pub meta: store::HubMeta,
    /// DM conversation keys per member handle.
    pub dms: BTreeMap<String, Vec<String>>,
    /// Message count per conversation key.
    pub counts: BTreeMap<String, usize>,
}

/// What a key press asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubKeyOutcome {
    Close,
    Changed,
    Post { to: String, text: String },
}

#[derive(Debug, Clone)]
pub struct HubBrowserState {
    pub hub: store::Hub,
    pub person: String,
    pub snapshot: HubSnapshot,
    pub feed: Vec<store::Message>,
    pub expanded: BTreeSet<String>,
    pub selected: usize,
    pub draft: String,
    pub is_composing: bool,
    /// A draft keeps its destination even if the selected row disappears.
    compose_target: Option<String>,
    /// Feed lines scrolled up from the newest message.
    pub feed_scroll: usize,
    /// Result of the last post, shown in the post box.
    pub notice: Option<String>,
    pub window: ModalWindowState,
}

impl HubBrowserState {
    /// Purpose: Open the browser on a hub.
    /// Input: The hub, the person handle, and an optional `#channel` to select.
    /// Output: The state, or an error when the hub files cannot be read.
    pub fn open(hub: store::Hub, person: String, channel: Option<&str>) -> Result<Self> {
        let snapshot = load_snapshot(&hub)?;
        let mut state = Self {
            hub,
            person,
            snapshot,
            feed: Vec::new(),
            expanded: BTreeSet::new(),
            selected: 1,
            draft: String::new(),
            is_composing: false,
            compose_target: None,
            feed_scroll: 0,
            notice: None,
            window: ModalWindowState::new(),
        };
        if let Some(name) = channel.and_then(store::normalize_channel) {
            let target = Row::Channel(name);
            if let Some(idx) = state.rows().iter().position(|row| *row == target) {
                state.selected = idx;
            }
        }
        state.reload()?;
        Ok(state)
    }

    pub fn rows(&self) -> Vec<Row> {
        tree_rows(&self.snapshot, &self.expanded)
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.rows().get(self.selected).cloned()
    }

    /// Purpose: Refresh the hub without a selection change when the row still exists.
    /// Input: The browser state and the hub files.
    /// Output: The same selected row, or a valid fallback if the row no longer exists.
    pub fn reload(&mut self) -> Result<()> {
        let selected_row = self.selected_row();
        self.snapshot = load_snapshot(&self.hub)?;
        let rows = self.rows();
        self.selected = match rows
            .iter()
            .position(|row| Some(row) == selected_row.as_ref())
        {
            Some(idx) => idx,
            None => {
                self.feed_scroll = 0;
                clamp_selection(&rows, self.selected)
            }
        };
        self.feed = match rows.get(self.selected) {
            Some(row) => load_feed(&self.hub, &self.snapshot, row)?,
            None => Vec::new(),
        };
        Ok(())
    }

    /// Where the person's draft goes, or the selected row when no draft is active.
    pub fn post_target(&self) -> Option<String> {
        self.compose_target
            .clone()
            .or_else(|| post_target(&self.selected_row()?, &self.person))
    }

    /// Purpose: Accept a post result without loss of a failed draft.
    /// Input: The destination and the post result from the controller.
    /// Output: A status notice. Only a successful post clears the draft.
    pub fn finish_post(&mut self, to: &str, result: Result<String>) {
        self.notice = Some(match result {
            Ok(_) => {
                self.draft.clear();
                format!("Posted to {to} as @{}.", self.person)
            }
            Err(error) => format!("Not posted: {error}"),
        });
    }

    /// Purpose: Apply one key press to the browser.
    /// Input: The key event.
    /// Output: What the caller must do next. A selection change reloads the feed.
    pub fn handle_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        if self.is_composing {
            return self.handle_compose_key(key);
        }
        let rows = self.rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return HubKeyOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(&rows, -1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(&rows, 1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.expand(&rows),
            KeyCode::Left | KeyCode::Char('h') => self.collapse(&rows),
            KeyCode::PageUp => self.feed_scroll = self.feed_scroll.saturating_add(10),
            KeyCode::PageDown => self.feed_scroll = self.feed_scroll.saturating_sub(10),
            KeyCode::Tab | KeyCode::Char('i') if self.post_target().is_some() => {
                self.compose_target = self.post_target();
                self.is_composing = true;
                self.notice = None;
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.is_composing {
            self.draft.push_str(&text.replace(['\r', '\n'], " "));
        }
    }

    /// Purpose: Edit or submit the draft without a destination change.
    /// Input: A key event while the post box has focus.
    /// Output: The next browser action. An empty draft releases its target on exit.
    fn handle_compose_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.is_composing = false;
                if self.draft.is_empty() {
                    self.compose_target = None;
                }
            }
            KeyCode::Backspace => {
                self.draft.pop();
            }
            KeyCode::Enter => {
                let text = self.draft.trim().to_string();
                if let (false, Some(to)) = (text.is_empty(), self.post_target()) {
                    return HubKeyOutcome::Post { to, text };
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.draft.push(c);
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    fn move_selection(&mut self, rows: &[Row], step: isize) {
        let mut idx = self.selected as isize;
        loop {
            idx += step;
            if idx < 0 || idx as usize >= rows.len() {
                return;
            }
            if rows[idx as usize].is_selectable() {
                self.select(idx as usize);
                return;
            }
        }
    }

    fn expand(&mut self, rows: &[Row]) {
        let Some(Row::Member(handle)) = rows.get(self.selected) else {
            return;
        };
        if self.snapshot.dms.get(handle).is_none_or(Vec::is_empty) {
            return;
        }
        if !self.expanded.insert(handle.clone()) {
            // Already open: step into the first DM.
            self.select(self.selected + 1);
        }
    }

    fn collapse(&mut self, rows: &[Row]) {
        match rows.get(self.selected) {
            Some(Row::Member(handle)) => {
                self.expanded.remove(handle);
            }
            Some(Row::Dm { owner, .. }) => {
                let parent = Row::Member(owner.clone());
                if let Some(idx) = rows.iter().position(|row| *row == parent) {
                    self.select(idx);
                }
            }
            _ => {}
        }
    }

    fn select(&mut self, idx: usize) {
        self.selected = idx;
        self.feed_scroll = 0;
        if let Some(row) = self.rows().get(idx) {
            self.feed = load_feed(&self.hub, &self.snapshot, row).unwrap_or_default();
        }
    }
}

/// Purpose: Read hub metadata, DM lists, and message counts.
/// Input: The hub.
/// Output: A snapshot. It has a count for every channel and every DM.
pub fn load_snapshot(hub: &store::Hub) -> Result<HubSnapshot> {
    let mut meta = hub.meta()?;
    for member in &mut meta.members {
        member.active = member.is_live();
    }
    let mut counts = BTreeMap::new();
    for channel in &meta.channels {
        let conv = format!("#{}", channel.name);
        counts.insert(conv.clone(), hub.messages(&conv, 0)?.len());
    }
    let mut dms = BTreeMap::new();
    for member in &meta.members {
        let convs = hub.dm_convs_for(&member.handle)?;
        for conv in &convs {
            if !counts.contains_key(conv) {
                counts.insert(conv.clone(), hub.messages(conv, 0)?.len());
            }
        }
        dms.insert(member.handle.clone(), convs);
    }
    Ok(HubSnapshot { meta, dms, counts })
}

/// Purpose: Lay out the tree rows.
/// Input: The snapshot and the handles of the expanded members.
/// Output: Channel rows, then member rows. An expanded member has its DM rows under it.
pub fn tree_rows(snapshot: &HubSnapshot, expanded: &BTreeSet<String>) -> Vec<Row> {
    let mut rows = vec![Row::Section("Channels")];
    for channel in &snapshot.meta.channels {
        rows.push(Row::Channel(channel.name.clone()));
    }
    rows.push(Row::Section("Members"));
    for member in &snapshot.meta.members {
        rows.push(Row::Member(member.handle.clone()));
        if !expanded.contains(&member.handle) {
            continue;
        }
        for conv in snapshot.dms.get(&member.handle).into_iter().flatten() {
            let Some((a, b)) = store::split_dm_conv(conv) else {
                continue;
            };
            let peer = if a == member.handle { b } else { a };
            rows.push(Row::Dm {
                owner: member.handle.clone(),
                peer: peer.to_string(),
                conv: conv.clone(),
            });
        }
    }
    rows
}

/// Purpose: Load the feed of one row, oldest message first.
/// Input: The hub, the snapshot, and the row.
/// Output: A channel's messages, one DM, or every DM of a member, merged by time.
pub fn load_feed(
    hub: &store::Hub,
    snapshot: &HubSnapshot,
    row: &Row,
) -> Result<Vec<store::Message>> {
    let convs: Vec<String> = match row {
        Row::Section(_) => Vec::new(),
        Row::Channel(name) => vec![format!("#{name}")],
        Row::Member(handle) => snapshot.dms.get(handle).cloned().unwrap_or_default(),
        Row::Dm { conv, .. } => vec![conv.clone()],
    };
    let mut messages = Vec::new();
    for conv in &convs {
        messages.extend(hub.messages(conv, 0)?);
    }
    messages.sort_by_key(|m| m.ts_unix_ms);
    let skip = messages.len().saturating_sub(FEED_LIMIT);
    Ok(messages.split_off(skip))
}

/// Where a post from `person` goes for `row`. A DM between two other members is read only.
pub fn post_target(row: &Row, person: &str) -> Option<String> {
    match row {
        Row::Section(_) => None,
        Row::Channel(name) => Some(format!("#{name}")),
        Row::Member(handle) if handle != person => Some(format!("@{handle}")),
        Row::Member(_) => None,
        Row::Dm { owner, peer, .. } if owner == person => Some(format!("@{peer}")),
        Row::Dm { owner, peer, .. } if peer == person => Some(format!("@{owner}")),
        Row::Dm { .. } => None,
    }
}

fn clamp_selection(rows: &[Row], selected: usize) -> usize {
    let idx = selected.min(rows.len().saturating_sub(1));
    if rows.get(idx).is_some_and(Row::is_selectable) {
        return idx;
    }
    rows.iter().position(Row::is_selectable).unwrap_or(0)
}

/// Purpose: Draw the browser.
/// Input: The buffer, the screen area, and the state.
/// Output: The popup rect, so the caller can mark it as an occluder.
pub fn render_hub_browser(
    buf: &mut Buffer,
    area: Rect,
    state: &mut HubBrowserState,
) -> Option<Rect> {
    let theme = Theme::current();
    let shortcuts = shortcuts(state);
    let title = format!("Hub {}", state.snapshot.meta.name);
    let config = ModalWindowConfig {
        title: &title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing::large(),
        fold_info: None,
    };
    let content = render_modal_window(buf, area, &mut state.window, &config, &theme)?.content;
    let tree_w = TREE_MAX_WIDTH.min(content.width / 3);
    let tree = Rect {
        width: tree_w,
        ..content
    };
    let divider_x = content.x + tree_w + 1;
    let feed = Rect {
        x: divider_x + 2,
        width: content.right().saturating_sub(divider_x + 2),
        ..content
    };
    render_tree(buf, tree, state, &theme);
    for y in content.y..content.bottom() {
        span_at(
            buf,
            divider_x,
            y,
            "│",
            Style::default().fg(theme.gray_dim),
            divider_x + 1,
        );
    }
    render_feed(buf, feed, state, &theme);
    state.window.popup_area
}

fn shortcuts(state: &HubBrowserState) -> Vec<Shortcut<'static>> {
    let labels: &[&'static str] = if state.is_composing {
        &["Enter send", "Esc stop typing"]
    } else {
        &[
            "↑↓ move",
            "→ open DMs",
            "← back",
            "Tab post",
            "PgUp/PgDn scroll",
            "Esc close",
        ]
    };
    labels
        .iter()
        .map(|label| Shortcut {
            label: (*label).into(),
            clickable: false,
            id: 0,
        })
        .collect()
}

fn render_tree(buf: &mut Buffer, area: Rect, state: &HubBrowserState, theme: &Theme) {
    let rows = state.rows();
    let snapshot = &state.snapshot;
    let list_h = area.height.saturating_sub(2) as usize;
    let first = state.selected.saturating_sub(list_h.saturating_sub(1));
    for (i, row) in rows.iter().enumerate().skip(first).take(list_h) {
        let y = area.y + (i - first) as u16;
        let is_selected = i == state.selected;
        let base = if is_selected {
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.bg_highlight)
        } else {
            Style::default().fg(theme.text_primary)
        };
        if is_selected {
            span_at(
                buf,
                area.x,
                y,
                &" ".repeat(area.width as usize),
                base,
                area.right(),
            );
        }
        let (left, right, left_style) = tree_row_text(row, snapshot, &state.expanded, theme, base);
        span_at(buf, area.x, y, &left, left_style, area.right());
        let right_x = area.right().saturating_sub(right.width() as u16);
        span_at(
            buf,
            right_x,
            y,
            &right,
            base.fg(theme.gray_bright),
            area.right(),
        );
    }
    let tokens: u64 = snapshot.meta.tokens.values().sum();
    let footer = format!("~{tokens} tokens to agents");
    span_at(
        buf,
        area.x,
        area.bottom().saturating_sub(1),
        &footer,
        Style::default().fg(theme.gray),
        area.right(),
    );
}

fn tree_row_text(
    row: &Row,
    snapshot: &HubSnapshot,
    expanded: &BTreeSet<String>,
    theme: &Theme,
    base: Style,
) -> (String, String, Style) {
    let count = |conv: &str| snapshot.counts.get(conv).copied().unwrap_or(0).to_string();
    match row {
        Row::Section(name) => (
            name.to_string(),
            String::new(),
            Style::default()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::BOLD),
        ),
        Row::Channel(name) => (format!("  #{name}"), count(&format!("#{name}")), base),
        Row::Member(handle) => {
            let member = snapshot.meta.member(handle);
            let has_dms = snapshot.dms.get(handle).is_some_and(|d| !d.is_empty());
            let arrow = match (has_dms, expanded.contains(handle)) {
                (false, _) => " ",
                (true, false) => "▸",
                (true, true) => "▾",
            };
            let is_person = member.is_some_and(|m| m.kind == MemberKind::Person);
            let is_live = member.is_some_and(|m| m.active);
            let dot = match (is_person, is_live) {
                (true, _) => "",
                (false, true) => " ●",
                (false, false) => " ○",
            };
            let label = member.map(|m| m.label.as_str()).unwrap_or("");
            let style = if is_live {
                base.fg(theme.accent_user)
            } else {
                base.fg(theme.gray)
            };
            (format!("{arrow} @{handle}{dot}"), label.to_string(), style)
        }
        Row::Dm { peer, conv, .. } => (format!("    ↳ @{peer}"), count(conv), base),
    }
}

fn render_feed(buf: &mut Buffer, area: Rect, state: &HubBrowserState, theme: &Theme) {
    let Some(row) = state.selected_row() else {
        return;
    };
    let header = feed_header(&row, &state.snapshot);
    span_at(
        buf,
        area.x,
        area.y,
        &header,
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
        area.right(),
    );
    let feed_h = area.height.saturating_sub(COMPOSE_ROWS + 2) as usize;
    let persons: BTreeSet<String> = state
        .snapshot
        .meta
        .members
        .iter()
        .filter(|m| m.kind == MemberKind::Person)
        .map(|m| m.handle.clone())
        .collect();
    let show_peer = matches!(row, Row::Member(_));
    let lines = feed_lines(&state.feed, area.width as usize, show_peer, &persons);
    let end = lines
        .len()
        .saturating_sub(state.feed_scroll.min(lines.len()));
    let start = end.saturating_sub(feed_h);
    if lines.is_empty() {
        span_at(
            buf,
            area.x,
            area.y + 2,
            "No messages yet.",
            Style::default().fg(theme.gray),
            area.right(),
        );
    }
    for (i, line) in lines[start..end].iter().enumerate() {
        let y = area.y + 2 + i as u16;
        let mut x = area.x;
        for (text, kind) in line {
            let style = match kind {
                LinePart::Time => Style::default().fg(theme.gray),
                LinePart::Agent => Style::default()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
                LinePart::Person => Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
                LinePart::Text => Style::default().fg(theme.text_primary),
            };
            span_at(buf, x, y, text, style, area.right());
            x += text.width() as u16;
        }
    }
    render_compose(buf, area, state, theme);
}

fn feed_header(row: &Row, snapshot: &HubSnapshot) -> String {
    match row {
        Row::Section(_) => String::new(),
        Row::Channel(name) => {
            let purpose = snapshot
                .meta
                .channels
                .iter()
                .find(|c| &c.name == name)
                .map(|c| c.purpose.as_str())
                .unwrap_or("");
            if purpose.is_empty() {
                format!("#{name}")
            } else {
                format!("#{name}  ·  {purpose}")
            }
        }
        Row::Member(handle) => format!("@{handle}  ·  all DMs"),
        Row::Dm { owner, peer, .. } => format!("DM  @{owner} ↔ @{peer}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinePart {
    Time,
    Agent,
    Person,
    Text,
}

/// Purpose: Wrap feed messages into screen lines.
/// Input: The messages, the width, whether to show each DM peer, and the person handles.
/// Output: Lines of (text, part) spans. The first line of a message has the time and sender.
fn feed_lines(
    messages: &[store::Message],
    width: usize,
    show_peer: bool,
    persons: &BTreeSet<String>,
) -> Vec<Vec<(String, LinePart)>> {
    let mut lines = Vec::new();
    if width == 0 {
        return lines;
    }
    for message in messages {
        let time = chrono::DateTime::from_timestamp_millis(message.ts_unix_ms as i64)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let sender_part = if persons.contains(&message.from) {
            LinePart::Person
        } else {
            LinePart::Agent
        };
        let mut sender = format!("@{}", message.from);
        if show_peer && let Some((a, b)) = store::split_dm_conv(&message.conv) {
            let to = if a == message.from { b } else { a };
            sender.push_str(&format!(" → @{to}"));
        }
        let prefix_w = time.width() + 1 + sender.width() + 2;
        if prefix_w + 10 > width {
            let header = format!("{time} {sender}");
            for chunk in textwrap::wrap(&header, width) {
                lines.push(vec![(chunk.into_owned(), sender_part)]);
            }
            for chunk in textwrap::wrap(&message.text, width) {
                lines.push(vec![(chunk.into_owned(), LinePart::Text)]);
            }
            continue;
        }
        let text_w = width - prefix_w;
        let wrapped = textwrap::wrap(&message.text, text_w);
        for (i, chunk) in wrapped.iter().enumerate() {
            if i == 0 {
                lines.push(vec![
                    (format!("{time} "), LinePart::Time),
                    (format!("{sender}  "), sender_part),
                    (chunk.to_string(), LinePart::Text),
                ]);
            } else {
                lines.push(vec![(
                    " ".repeat(prefix_w) + chunk.as_ref(),
                    LinePart::Text,
                )]);
            }
        }
    }
    lines
}

fn render_compose(buf: &mut Buffer, area: Rect, state: &HubBrowserState, theme: &Theme) {
    let top = area.bottom().saturating_sub(COMPOSE_ROWS);
    let rule = "─".repeat(area.width as usize);
    span_at(
        buf,
        area.x,
        top,
        &rule,
        Style::default().fg(theme.gray_dim),
        area.right(),
    );
    let (text, style) = match (state.post_target(), state.is_composing) {
        (None, _) => (
            "Read only: a DM between two other members.".to_string(),
            Style::default().fg(theme.gray),
        ),
        (Some(to), true) => (
            format!("{to} › {}█", state.draft),
            Style::default().fg(theme.text_primary),
        ),
        (Some(to), false) => match &state.notice {
            Some(notice) => (notice.clone(), Style::default().fg(theme.gray_bright)),
            None => (
                format!("Tab to post to {to} as @{}", state.person),
                Style::default().fg(theme.gray),
            ),
        },
    };
    span_at(buf, area.x, top + 1, &text, style, area.right());
}

fn span_at(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max_x: u16) {
    if max_x <= x {
        return;
    }
    buf.set_span_safe(
        x,
        y,
        &ratatui::text::Span::styled(text.to_string(), style),
        max_x - x,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn hub_with_dms() -> (tempfile::TempDir, store::Hub) {
        let root = tempfile::tempdir().unwrap();
        let hub = store::Hub::create(root.path(), "demo").unwrap();
        hub.update_meta(|meta| {
            for handle in ["front", "back", "me"] {
                meta.members.push(store::Member {
                    handle: handle.to_string(),
                    label: "main".to_string(),
                    kind: if handle == "me" {
                        MemberKind::Person
                    } else {
                        MemberKind::Agent
                    },
                    session_id: None,
                    active: true,
                    disconnected: false,
                    joined_ms: 0,
                    pid: None,
                });
            }
        })
        .unwrap();
        for (from, conv, text) in [
            ("back", store::dm_conv("back", "front"), "write add.py"),
            ("front", store::dm_conv("back", "front"), "done: 5"),
            ("me", store::dm_conv("me", "front"), "status?"),
            ("me", "#general".to_string(), "hello"),
        ] {
            hub.append(&store::Message {
                id: text.to_string(),
                conv,
                from: from.to_string(),
                text: text.to_string(),
                reply_to: None,
                mentions: Vec::new(),
                images: Vec::new(),
                ts_unix_ms: store::now_ms(),
            })
            .unwrap();
        }
        (root, hub)
    }

    #[test]
    fn tree_lists_channels_then_members_and_expands_dms() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub, "me".to_string(), None).unwrap();
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
        assert_eq!(state.feed.len(), 1);
        assert_eq!(state.post_target().as_deref(), Some("#general"));

        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(Row::Member("front".to_string())));
        assert_eq!(state.feed.len(), 3, "a member row merges all of its DMs");

        state.handle_key(&key(KeyCode::Right));
        state.handle_key(&key(KeyCode::Right));
        let Some(Row::Dm { peer, .. }) = state.selected_row() else {
            panic!("expected a DM row");
        };
        assert_eq!(peer, "back");
        assert_eq!(state.feed.len(), 2);
        assert_eq!(
            state.post_target(),
            None,
            "a DM between two agents is read only"
        );

        state.handle_key(&key(KeyCode::Left));
        assert_eq!(state.selected_row(), Some(Row::Member("front".to_string())));
        state.handle_key(&key(KeyCode::Left));
        assert!(!state.rows().iter().any(|row| matches!(row, Row::Dm { .. })));
    }

    #[test]
    fn compose_posts_to_the_selected_target() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub, "me".to_string(), Some("#general")).unwrap();
        assert_eq!(state.handle_key(&key(KeyCode::Tab)), HubKeyOutcome::Changed);
        assert!(state.is_composing);
        for c in "hi all".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "#general".to_string(),
                text: "hi all".to_string()
            }
        );
        assert_eq!(state.draft, "hi all", "the post has not succeeded yet");
        state.finish_post("#general", Ok("message-id".to_string()));
        assert!(state.draft.is_empty());
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.handle_key(&key(KeyCode::Esc)), HubKeyOutcome::Close);
    }

    #[test]
    fn reload_preserves_row_identity_and_compose_destination_after_channel_insert() {
        for selected in [
            Row::Channel("general".to_string()),
            Row::Member("front".to_string()),
            Row::Dm {
                owner: "front".to_string(),
                peer: "me".to_string(),
                conv: store::dm_conv("front", "me"),
            },
        ] {
            let (_root, hub) = hub_with_dms();
            let mut state = HubBrowserState::open(hub, "me".to_string(), None).unwrap();
            state.expanded.insert("front".to_string());
            let idx = state
                .rows()
                .iter()
                .position(|row| *row == selected)
                .unwrap();
            state.select(idx);
            let target = state.post_target().unwrap();
            let feed = state.feed.clone();
            state.handle_key(&key(KeyCode::Tab));
            state.handle_paste("keep this destination");
            state.feed_scroll = 10;
            state
                .hub
                .update_meta(|meta| {
                    meta.channels.insert(
                        0,
                        store::Channel {
                            name: "alerts".to_string(),
                            purpose: String::new(),
                            created_ms: 0,
                        },
                    );
                })
                .unwrap();

            state.reload().unwrap();

            assert_eq!(state.selected_row(), Some(selected));
            assert_eq!(state.selected, idx + 1);
            assert_eq!(state.feed, feed);
            assert_eq!(state.feed_scroll, 10);
            assert_eq!(
                state.handle_key(&key(KeyCode::Enter)),
                HubKeyOutcome::Post {
                    to: target,
                    text: "keep this destination".to_string(),
                }
            );
        }
    }

    #[test]
    fn reload_preserves_dm_row_after_an_earlier_dm_appears() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub, "me".to_string(), None).unwrap();
        state.expanded.insert("front".to_string());
        let selected = Row::Dm {
            owner: "front".to_string(),
            peer: "me".to_string(),
            conv: store::dm_conv("front", "me"),
        };
        let idx = state
            .rows()
            .iter()
            .position(|row| *row == selected)
            .unwrap();
        state.select(idx);
        state
            .hub
            .append(&store::Message {
                id: "new-dm".to_string(),
                conv: store::dm_conv("aaa", "front"),
                from: "aaa".to_string(),
                text: "new conversation".to_string(),
                reply_to: None,
                mentions: Vec::new(),
                images: Vec::new(),
                ts_unix_ms: store::now_ms(),
            })
            .unwrap();

        state.reload().unwrap();

        assert_eq!(state.selected_row(), Some(selected));
        assert_eq!(state.selected, idx + 1);
        assert_eq!(state.post_target().as_deref(), Some("@front"));
        assert_eq!(state.feed[0].text, "status?");
    }

    #[test]
    fn reload_does_not_redirect_a_draft_when_its_row_disappears() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub, "me".to_string(), None).unwrap();
        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(Row::Member("front".to_string())));
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("for front only");
        state
            .hub
            .update_meta(|meta| {
                meta.members.retain(|member| member.handle != "front");
            })
            .unwrap();

        state.reload().unwrap();

        assert_eq!(state.selected_row(), Some(Row::Member("back".to_string())));
        assert_eq!(state.post_target().as_deref(), Some("@front"));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "@front".to_string(),
                text: "for front only".to_string(),
            }
        );
    }

    #[test]
    fn compose_keeps_a_paused_draft_target_until_the_draft_is_empty() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub, "me".to_string(), None).unwrap();
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("hello");
        state.handle_key(&key(KeyCode::Esc));
        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(Row::Member("front".to_string())));
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "#general".to_string(),
                text: "hello".to_string(),
            }
        );
        state.finish_post("#general", Ok("message-id".to_string()));
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.post_target().as_deref(), Some("@front"));
    }

    #[test]
    fn feed_lines_keep_message_bodies_visible_with_long_sender_headers() {
        let from = "a".repeat(64);
        let message = store::Message {
            id: "long-sender".to_string(),
            conv: store::dm_conv(&from, &"b".repeat(64)),
            from: from.clone(),
            text: "one two three four five six".to_string(),
            reply_to: None,
            mentions: Vec::new(),
            images: Vec::new(),
            ts_unix_ms: 0,
        };
        for width in [1, 10, 20, 44] {
            for show_peer in [false, true] {
                let lines = feed_lines(
                    std::slice::from_ref(&message),
                    width,
                    show_peer,
                    &BTreeSet::from([from.clone()]),
                );
                for line in &lines {
                    assert!(line.iter().map(|(text, _)| text.width()).sum::<usize>() <= width);
                }
                let visible_body = lines
                    .iter()
                    .flatten()
                    .filter(|(_, part)| *part == LinePart::Text)
                    .map(|(text, _)| text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                assert_eq!(
                    visible_body.split_whitespace().collect::<String>(),
                    message.text.split_whitespace().collect::<String>(),
                );
                assert!(
                    lines
                        .iter()
                        .flatten()
                        .any(|(_, part)| *part == LinePart::Person)
                );
            }
        }
        assert!(feed_lines(&[message], 0, false, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn feed_lines_wrap_and_name_the_dm_peer() {
        let message = store::Message {
            id: "1".to_string(),
            conv: store::dm_conv("a", "b"),
            from: "a".to_string(),
            text: "one two three four five six".to_string(),
            reply_to: None,
            mentions: Vec::new(),
            images: Vec::new(),
            ts_unix_ms: 0,
        };
        let lines = feed_lines(&[message], 30, true, &BTreeSet::new());
        assert!(lines.len() > 1);
        assert!(lines[0][1].0.contains("@a → @b"));
    }
}
