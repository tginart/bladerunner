//! Hub browser: a full-screen, Slack-style view of one agent hub (`/hub open`).
//!
//! The view shows the hub from the perspective of one member. The left column
//! has the channels, then the direct messages of that member. The main pane
//! shows the selected conversation as a chat thread, with a post box at the
//! bottom. A chip at the top right selects the perspective: the person (you)
//! by default, or any other member. Only the person can post. Only the
//! person's read cursor moves when the view shows a conversation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;
use xai_grok_tools::implementations::grok_build::agent_hub::{self, MemberKind, store};

use crate::render::SafeBuf;
use crate::theme::Theme;

/// Messages that one feed shows.
const FEED_LIMIT: usize = 300;
/// Widest the sidebar gets.
const SIDEBAR_MAX_WIDTH: u16 = 30;
/// Rows of the post box: top border, text, bottom border.
const COMPOSE_ROWS: u16 = 3;
/// Messages from one sender closer than this share one header.
const GROUP_GAP_MS: u64 = 5 * 60 * 1000;
/// Feed lines that PgUp and PgDn scroll.
const PAGE_LINES: usize = 10;
/// Columns of the message body indent under a sender header.
const BODY_INDENT: usize = 2;

/// One row of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Section(&'static str),
    Channel(String),
    /// A DM between the perspective member and `peer`.
    Dm {
        peer: String,
        conv: String,
    },
}

impl Row {
    fn is_selectable(&self) -> bool {
        !matches!(self, Row::Section(_))
    }

    /// The conversation key of the row, or `None` for a section.
    pub fn conv(&self) -> Option<String> {
        match self {
            Row::Section(_) => None,
            Row::Channel(name) => Some(format!("#{name}")),
            Row::Dm { conv, .. } => Some(conv.clone()),
        }
    }
}

/// Hub data read from disk for one perspective.
#[derive(Debug, Clone)]
pub struct HubSnapshot {
    pub meta: store::HubMeta,
    /// DM peers of the perspective member, in member order.
    pub dm_peers: Vec<String>,
    /// Unread message count per conversation key.
    pub unread_counts: BTreeMap<String, usize>,
    /// Read cursor of the perspective member per conversation key.
    pub read: BTreeMap<String, usize>,
}

impl HubSnapshot {
    /// Messages in `conv` after the read cursor of the perspective member,
    /// not counting the messages that member sent.
    pub fn unread(&self, conv: &str) -> usize {
        self.unread_counts.get(conv).copied().unwrap_or(0)
    }
}

/// What an input event asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubKeyOutcome {
    Close,
    Changed,
    Post { to: String, text: String },
}

/// Screen areas of the last frame, for mouse hits.
#[derive(Debug, Clone, Default)]
struct HitAreas {
    sidebar: Rect,
    /// Screen row and row index of each visible sidebar row.
    rows: Vec<(u16, usize)>,
    chip: Rect,
    picker: Vec<(Rect, String)>,
    compose: Rect,
}

#[derive(Debug, Clone)]
pub struct HubBrowserState {
    pub hub: store::Hub,
    /// The handle of the person at the keyboard.
    pub person: String,
    /// The member whose view the browser shows.
    pub perspective: String,
    pub snapshot: HubSnapshot,
    pub feed: Vec<store::Message>,
    /// Feed index of the first message that the perspective member had not read.
    pub first_unread: Option<usize>,
    pub selected: usize,
    pub draft: String,
    pub is_composing: bool,
    /// A draft keeps its destination even if the selected row disappears.
    compose_target: Option<String>,
    /// Feed lines scrolled up from the newest message.
    pub feed_scroll: usize,
    /// Result of the last post, shown over the post box.
    pub notice: Option<String>,
    /// The highlighted index of the open perspective picker.
    pub picker: Option<usize>,
    hits: HitAreas,
}

impl HubBrowserState {
    /// Purpose: Open the browser on a hub, as the person.
    /// Input: The hub, the person handle, and an optional `#channel` to select.
    /// Output: The state, or an error when the hub files cannot be read.
    pub fn open(hub: store::Hub, person: String, channel: Option<&str>) -> Result<Self> {
        let snapshot = load_snapshot(&hub, &person, &person)?;
        let mut state = Self {
            hub,
            perspective: person.clone(),
            person,
            snapshot,
            feed: Vec::new(),
            first_unread: None,
            selected: 1,
            draft: String::new(),
            is_composing: false,
            compose_target: None,
            feed_scroll: 0,
            notice: None,
            picker: None,
            hits: HitAreas::default(),
        };
        let rows = state.rows();
        let target = channel.and_then(store::normalize_channel).map(Row::Channel);
        let idx = target
            .and_then(|target| rows.iter().position(|row| *row == target))
            .unwrap_or_else(|| clamp_selection(&rows, 1));
        state.select(idx);
        Ok(state)
    }

    pub fn rows(&self) -> Vec<Row> {
        sidebar_rows(&self.snapshot, &self.perspective)
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.rows().get(self.selected).cloned()
    }

    pub fn is_person_view(&self) -> bool {
        self.perspective == self.person
    }

    /// Purpose: Read the hub again and keep the selected row when it still exists.
    /// Input: The browser state and the hub files.
    /// Output: Ok, or the read error. A row that is gone selects a valid fallback.
    pub fn reload(&mut self) -> Result<()> {
        let selected_row = self.selected_row();
        self.snapshot = load_snapshot(&self.hub, &self.perspective, &self.person)?;
        let rows = self.rows();
        match rows
            .iter()
            .position(|row| Some(row) == selected_row.as_ref())
        {
            Some(idx) => {
                self.selected = idx;
                self.load_feed(false)?;
            }
            None => {
                self.selected = clamp_selection(&rows, self.selected);
                self.feed_scroll = 0;
                self.load_feed(true)?;
            }
        }
        Ok(())
    }

    /// Where the person's draft goes, or `None` when the view is read only.
    pub fn post_target(&self) -> Option<String> {
        if !self.is_person_view() {
            return None;
        }
        self.compose_target
            .clone()
            .or_else(|| post_target(&self.selected_row()?))
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

    /// Purpose: Show the hub as another member sees it.
    /// Input: The member handle.
    /// Output: The new view. The selected channel stays when it exists.
    pub fn set_perspective(&mut self, handle: &str) -> Result<()> {
        self.picker = None;
        if handle == self.perspective {
            return Ok(());
        }
        self.perspective = handle.to_string();
        self.is_composing = false;
        self.notice = None;
        let kept_row = self.selected_row();
        self.reload()?;
        // A row index means nothing in another member's sidebar, so go to the top.
        if self.selected_row() != kept_row {
            self.selected = clamp_selection(&self.rows(), 0);
        }
        // The unread marker belongs to the member whose view this is.
        self.load_feed(true)
    }

    /// The members that the perspective picker lists: the person first.
    pub fn picker_members(&self) -> Vec<String> {
        let mut handles = vec![self.person.clone()];
        for member in &self.snapshot.meta.members {
            if member.handle != self.person {
                handles.push(member.handle.clone());
            }
        }
        handles
    }

    /// Purpose: Apply one key press to the browser.
    /// Input: The key event.
    /// Output: What the caller must do next.
    pub fn handle_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if self.is_composing {
            return self.handle_compose_key(key);
        }
        let rows = self.rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return HubKeyOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(&rows, -1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(&rows, 1),
            KeyCode::PageUp => self.feed_scroll = self.feed_scroll.saturating_add(PAGE_LINES),
            KeyCode::PageDown => self.feed_scroll = self.feed_scroll.saturating_sub(PAGE_LINES),
            KeyCode::End => self.feed_scroll = 0,
            KeyCode::Char('p') => self.open_picker(),
            KeyCode::Tab | KeyCode::Enter | KeyCode::Char('i') => self.start_compose(),
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.is_composing {
            self.draft.push_str(&text.replace(['\r', '\n'], " "));
        }
    }

    /// Purpose: Apply one mouse event to the browser.
    /// Input: The event kind and the screen position.
    /// Output: What the caller must do next.
    /// Note: The hit areas come from the last frame.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> HubKeyOutcome {
        match kind {
            MouseEventKind::ScrollUp => self.feed_scroll = self.feed_scroll.saturating_add(3),
            MouseEventKind::ScrollDown => self.feed_scroll = self.feed_scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => self.click(col, row),
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    fn click(&mut self, col: u16, row: u16) {
        if self.picker.is_some() {
            let hit = self
                .hits
                .picker
                .iter()
                .find(|(rect, _)| contains(*rect, col, row))
                .map(|(_, handle)| handle.clone());
            match hit {
                Some(handle) => self.apply_perspective(&handle),
                None => self.picker = None,
            }
            return;
        }
        if contains(self.hits.chip, col, row) {
            self.open_picker();
            return;
        }
        if contains(self.hits.compose, col, row) {
            self.start_compose();
            return;
        }
        if !contains(self.hits.sidebar, col, row) {
            return;
        }
        let hit = self
            .hits
            .rows
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, idx)| *idx);
        if let Some(idx) = hit
            && self.rows().get(idx).is_some_and(Row::is_selectable)
        {
            self.is_composing = false;
            self.select(idx);
        }
    }

    fn open_picker(&mut self) {
        let members = self.picker_members();
        let current = members.iter().position(|h| *h == self.perspective);
        self.picker = Some(current.unwrap_or(0));
    }

    fn handle_picker_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        let members = self.picker_members();
        let Some(idx) = self.picker else {
            return HubKeyOutcome::Changed;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('q') => self.picker = None,
            KeyCode::Up | KeyCode::Char('k') => self.picker = Some(idx.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.picker = Some((idx + 1).min(members.len().saturating_sub(1)));
            }
            KeyCode::Enter => {
                if let Some(handle) = members.get(idx) {
                    self.apply_perspective(&handle.clone());
                }
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    fn apply_perspective(&mut self, handle: &str) {
        if let Err(error) = self.set_perspective(handle) {
            self.notice = Some(format!("Could not load @{handle}: {error}"));
        }
    }

    fn start_compose(&mut self) {
        if let Some(target) = self.post_target() {
            self.compose_target = Some(target);
            self.is_composing = true;
            self.notice = None;
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

    fn select(&mut self, idx: usize) {
        self.selected = idx;
        self.feed_scroll = 0;
        if let Err(error) = self.load_feed(true) {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
    }

    /// Purpose: Load the feed of the selected row.
    /// Input: Whether to place the unread marker again.
    /// Output: Ok, or the read error. In the person view, the read cursor moves to the end.
    fn load_feed(&mut self, is_new_selection: bool) -> Result<()> {
        let Some(conv) = self.selected_row().and_then(|row| row.conv()) else {
            self.feed.clear();
            self.first_unread = None;
            return Ok(());
        };
        let mut messages = self.hub.messages(&conv, 0)?;
        let skip = messages.len().saturating_sub(FEED_LIMIT);
        self.feed = messages.split_off(skip);
        if is_new_selection {
            let read = self.snapshot.read.get(&conv).copied().unwrap_or(0);
            self.first_unread = self
                .feed
                .iter()
                .enumerate()
                .skip(read.saturating_sub(skip))
                .find(|(_, message)| message.from != self.perspective)
                .map(|(idx, _)| idx);
        }
        if self.is_person_view() {
            let count = self.feed.len() + skip;
            agent_hub::mark_read(&self.hub, &self.person, &conv, count)?;
            self.snapshot.read.insert(conv.clone(), count);
            self.snapshot.unread_counts.insert(conv, 0);
        }
        Ok(())
    }
}

/// Purpose: Read the hub as one member sees it.
/// Input: The hub, the perspective handle, and the person handle.
/// Output: A snapshot with a count for every channel and every DM row.
/// Note: The person sees every member as a DM row. Another member sees only
///     the DMs that it is in.
pub fn load_snapshot(hub: &store::Hub, perspective: &str, person: &str) -> Result<HubSnapshot> {
    let mut meta = hub.meta()?;
    for member in &mut meta.members {
        member.active = member.is_live();
    }
    let read = hub.cursors(perspective)?.read;
    let mut convs: Vec<String> = meta
        .channels
        .iter()
        .map(|channel| format!("#{}", channel.name))
        .collect();
    let mut conv_peers = BTreeSet::new();
    for conv in hub.dm_convs_for(perspective)? {
        let Some((a, b)) = store::split_dm_conv(&conv) else {
            continue;
        };
        let peer = if a == perspective { b } else { a };
        conv_peers.insert(peer.to_string());
        convs.push(conv.clone());
    }
    let mut unread_counts = BTreeMap::new();
    for conv in convs {
        let start = read.get(&conv).copied().unwrap_or(0);
        let unread = hub
            .messages(&conv, start)?
            .iter()
            .filter(|message| message.from != perspective)
            .count();
        unread_counts.insert(conv, unread);
    }
    let mut dm_peers = Vec::new();
    for member in &meta.members {
        let is_listed = perspective == person || conv_peers.contains(&member.handle);
        if member.handle != perspective && is_listed {
            dm_peers.push(member.handle.clone());
        }
    }
    for peer in conv_peers {
        if !dm_peers.contains(&peer) {
            dm_peers.push(peer);
        }
    }
    Ok(HubSnapshot {
        meta,
        dm_peers,
        unread_counts,
        read,
    })
}

/// Purpose: Lay out the sidebar rows.
/// Input: The snapshot and the perspective handle.
/// Output: The channel rows, then the DM rows of the perspective member.
pub fn sidebar_rows(snapshot: &HubSnapshot, perspective: &str) -> Vec<Row> {
    let mut rows = vec![Row::Section("Channels")];
    for channel in &snapshot.meta.channels {
        rows.push(Row::Channel(channel.name.clone()));
    }
    rows.push(Row::Section("Direct messages"));
    for peer in &snapshot.dm_peers {
        rows.push(Row::Dm {
            peer: peer.clone(),
            conv: store::dm_conv(perspective, peer),
        });
    }
    rows
}

/// Where a post from the person goes for `row`.
pub fn post_target(row: &Row) -> Option<String> {
    match row {
        Row::Section(_) => None,
        Row::Channel(name) => Some(format!("#{name}")),
        Row::Dm { peer, .. } => Some(format!("@{peer}")),
    }
}

fn clamp_selection(rows: &[Row], selected: usize) -> usize {
    let idx = selected.min(rows.len().saturating_sub(1));
    if rows.get(idx).is_some_and(Row::is_selectable) {
        return idx;
    }
    rows.iter().position(Row::is_selectable).unwrap_or(0)
}

fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom()
}

/// The style of each part of a feed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// An agent name. The number picks the name color.
    Agent(usize),
    Person,
    Time,
    Text,
    Mention,
    Muted,
    Rule,
    NewRule,
}

type FeedLine = Vec<(String, Part)>;

/// Names and channels that a feed highlights in message text.
struct FeedNames<'a> {
    persons: &'a BTreeSet<String>,
    /// `@handle`, `@here`, and `#channel` words.
    mentions: &'a BTreeSet<String>,
}

/// Purpose: Lay out feed messages as chat lines, oldest first.
/// Input: The messages, the width, the index of the first unread message,
///     the names to highlight, and the local date of today.
/// Output: Lines of (text, part) spans. No line is wider than `width`.
/// Method:
///     1. Put a day rule before the first message of each day.
///     2. Put a "new" rule before the first unread message. If a day rule is
///        already there, end the day rule with the "new" label.
///     3. Start a group with a sender and time header when the sender changes,
///        after a gap of GROUP_GAP_MS, or after a rule.
///     4. Indent and wrap each message body under its group header.
fn feed_lines(
    messages: &[store::Message],
    width: usize,
    first_unread: Option<usize>,
    names: &FeedNames<'_>,
    today: NaiveDate,
) -> Vec<FeedLine> {
    let mut lines: Vec<FeedLine> = Vec::new();
    if width < BODY_INDENT + 4 {
        return lines;
    }
    let mut prev: Option<&store::Message> = None;
    for (idx, message) in messages.iter().enumerate() {
        let time = local_time(message.ts_unix_ms);
        let is_new_day =
            prev.is_none_or(|p| local_time(p.ts_unix_ms).date_naive() != time.date_naive());
        let is_first_unread = first_unread == Some(idx);
        if is_new_day {
            if !lines.is_empty() {
                lines.push(Vec::new());
            }
            let rule = day_rule(time.date_naive(), today, width);
            lines.push(if is_first_unread {
                mark_new(rule)
            } else {
                rule
            });
        } else if is_first_unread {
            lines.push(new_rule(width));
        }
        let continues_group = !is_new_day
            && !is_first_unread
            && prev.is_some_and(|p| {
                p.from == message.from
                    && message.ts_unix_ms.saturating_sub(p.ts_unix_ms) < GROUP_GAP_MS
            });
        if !continues_group {
            if !is_new_day && !is_first_unread {
                lines.push(Vec::new());
            }
            lines.push(sender_header(message, &time, width, names));
        }
        push_body(&mut lines, message, width, names);
        prev = Some(message);
    }
    lines
}

fn local_time(ts_unix_ms: u64) -> DateTime<Local> {
    DateTime::from_timestamp_millis(ts_unix_ms as i64)
        .unwrap_or_default()
        .with_timezone(&Local)
}

fn day_rule(day: NaiveDate, today: NaiveDate, width: usize) -> FeedLine {
    let label = if day == today {
        "Today".to_string()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".to_string()
    } else {
        day.format("%A, %B %-d").to_string()
    };
    let label = format!(" {label} ");
    let side = width.saturating_sub(label.width()) / 2;
    let right = width.saturating_sub(side + label.width());
    vec![
        ("─".repeat(side), Part::Rule),
        (label, Part::Muted),
        ("─".repeat(right), Part::Rule),
    ]
}

fn new_rule(width: usize) -> FeedLine {
    let label = " new";
    vec![
        (
            "─".repeat(width.saturating_sub(label.width())),
            Part::NewRule,
        ),
        (label.to_string(), Part::NewRule),
    ]
}

/// End a day rule with the "new" label, so one rule marks both.
fn mark_new(mut rule: FeedLine) -> FeedLine {
    let label = " new";
    if let Some((tail, _)) = rule.last_mut() {
        let keep = tail.chars().count().saturating_sub(label.width());
        *tail = "─".repeat(keep);
    }
    rule.push((label.to_string(), Part::NewRule));
    rule
}

fn sender_header(
    message: &store::Message,
    time: &DateTime<Local>,
    width: usize,
    names: &FeedNames<'_>,
) -> FeedLine {
    let part = if names.persons.contains(&message.from) {
        Part::Person
    } else {
        Part::Agent(name_color_slot(&message.from))
    };
    let sender = format!("@{}", message.from);
    let time = format!("  {}", time.format("%-I:%M %p"));
    if sender.width() + time.width() > width {
        let mut fitted = String::new();
        for chunk in textwrap::wrap(&sender, width).into_iter().take(1) {
            fitted.push_str(&chunk);
        }
        return vec![(fitted, part)];
    }
    vec![(sender, part), (time, Part::Time)]
}

fn push_body(
    lines: &mut Vec<FeedLine>,
    message: &store::Message,
    width: usize,
    names: &FeedNames<'_>,
) {
    let indent = " ".repeat(BODY_INDENT);
    let body_width = width - BODY_INDENT;
    for paragraph in message.text.lines() {
        if paragraph.trim().is_empty() {
            lines.push(Vec::new());
            continue;
        }
        for chunk in textwrap::wrap(paragraph, body_width) {
            let mut line = vec![(indent.clone(), Part::Text)];
            line.extend(mention_spans(&chunk, names.mentions));
            lines.push(line);
        }
    }
    if !message.images.is_empty() {
        let count = message.images.len();
        let label = if count == 1 {
            "▣ 1 image".to_string()
        } else {
            format!("▣ {count} images")
        };
        lines.push(vec![(indent, Part::Text), (label, Part::Muted)]);
    }
}

/// Split `text` into plain spans and spans for known `@handle` and `#channel` words.
fn mention_spans(text: &str, mentions: &BTreeSet<String>) -> Vec<(String, Part)> {
    let mut spans: Vec<(String, Part)> = Vec::new();
    for word in text.split_inclusive(' ') {
        let core_end = word
            .trim_end_matches(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '/')))
            .len();
        let (core, tail) = word.split_at(core_end);
        if mentions.contains(core) {
            spans.push((core.to_string(), Part::Mention));
            push_text(&mut spans, tail);
        } else {
            push_text(&mut spans, word);
        }
    }
    spans
}

fn push_text(spans: &mut Vec<(String, Part)>, text: &str) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some((last, Part::Text)) => last.push_str(text),
        _ => spans.push((text.to_string(), Part::Text)),
    }
}

/// A stable color slot for an agent name.
fn name_color_slot(handle: &str) -> usize {
    handle.bytes().map(usize::from).sum::<usize>()
}

/// Purpose: Draw the browser over the full area.
/// Input: The buffer, the area, and the state. The state keeps the hit areas.
/// Output: The area, so the caller can mark it as an occluder.
pub fn render_hub_browser(buf: &mut Buffer, area: Rect, state: &mut HubBrowserState) -> Rect {
    let theme = Theme::current();
    state.hits = HitAreas::default();
    if area.width < 30 || area.height < 10 {
        Clear.render(area, buf);
        fill(buf, area, Style::default().bg(theme.bg_base));
        let text = "Make the terminal larger to show the hub.";
        span_at(buf, area.x, area.y, text, theme.muted(), area.right());
        return area;
    }
    Clear.render(area, buf);
    fill(buf, area, Style::default().bg(theme.bg_base));
    let top_bar = Rect { height: 1, ..area };
    let footer = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let body = Rect {
        y: area.y + 1,
        height: area.height - 2,
        ..area
    };
    let sidebar_w = SIDEBAR_MAX_WIDTH.min(body.width / 3);
    let sidebar = Rect {
        width: sidebar_w,
        ..body
    };
    let main = Rect {
        x: body.x + sidebar_w,
        width: body.width - sidebar_w,
        ..body
    };
    render_top_bar(buf, top_bar, state, &theme);
    render_sidebar(buf, sidebar, state, &theme);
    render_main(buf, main, state, &theme);
    render_footer(buf, footer, state, &theme);
    if state.picker.is_some() {
        render_picker(buf, area, state, &theme);
    }
    area
}

fn render_top_bar(buf: &mut Buffer, bar: Rect, state: &mut HubBrowserState, theme: &Theme) {
    let bar_bg = theme.bg_dark;
    fill(buf, bar, Style::default().bg(bar_bg));
    let meta = &state.snapshot.meta;
    let live = meta.members.iter().filter(|m| m.active).count();
    let mut x = bar.x + 1;
    x = span_at(
        buf,
        x,
        bar.y,
        "◆ ",
        Style::default().fg(theme.accent_verify).bg(bar_bg),
        bar.right(),
    );
    x = span_at(
        buf,
        x,
        bar.y,
        &meta.name,
        Style::default()
            .fg(theme.text_primary)
            .bg(bar_bg)
            .add_modifier(Modifier::BOLD),
        bar.right(),
    );
    let stats = format!(
        "   {} · {} · {live} live",
        plural(meta.channels.len(), "channel"),
        plural(meta.members.len(), "member"),
    );
    span_at(buf, x, bar.y, &stats, theme.muted().bg(bar_bg), bar.right());

    let who = if state.is_person_view() {
        "You".to_string()
    } else {
        format!("@{}", state.perspective)
    };
    let chip = format!(" ◉ {who} ▾ ");
    let label = "Viewing as ";
    let chip_w = chip.width() as u16;
    let label_w = label.width() as u16;
    let chip_x = bar.right().saturating_sub(chip_w + 1);
    let label_x = chip_x.saturating_sub(label_w);
    span_at(buf, label_x, bar.y, label, theme.muted().bg(bar_bg), chip_x);
    let chip_style = if state.is_person_view() {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_highlight)
    } else {
        Style::default().fg(theme.bg_base).bg(theme.accent_plan)
    };
    span_at(
        buf,
        chip_x,
        bar.y,
        &chip,
        chip_style.add_modifier(Modifier::BOLD),
        bar.right(),
    );
    state.hits.chip = Rect {
        x: label_x,
        y: bar.y,
        width: label_w + chip_w,
        height: 1,
    };
}

fn render_sidebar(buf: &mut Buffer, area: Rect, state: &mut HubBrowserState, theme: &Theme) {
    let side_bg = theme.bg_dark;
    fill(buf, area, Style::default().bg(side_bg));
    state.hits.sidebar = area;
    let rows = state.rows();
    let list_h = area.height.saturating_sub(3) as usize;
    let screen_rows = sidebar_screen_rows(&rows);
    let selected_line = screen_rows
        .iter()
        .position(|line| *line == Some(state.selected))
        .unwrap_or(0);
    let first = (selected_line + 1).saturating_sub(list_h);
    for (offset, line) in screen_rows.iter().skip(first).take(list_h).enumerate() {
        let Some(idx) = *line else {
            continue;
        };
        let y = area.y + 1 + offset as u16;
        let row_area = Rect {
            y,
            height: 1,
            ..area
        };
        render_sidebar_row(
            buf,
            row_area,
            &rows[idx],
            idx == state.selected,
            state,
            theme,
        );
        state.hits.rows.push((y, idx));
    }
    let tokens: u64 = state.snapshot.meta.tokens.values().sum();
    if tokens == 0 {
        return;
    }
    let footer = format!("  ~{tokens} tokens to agents");
    span_at(
        buf,
        area.x,
        area.bottom().saturating_sub(1),
        &footer,
        Style::default().fg(theme.gray).bg(side_bg),
        area.right(),
    );
}

/// The sidebar screen lines: a row index, or `None` for the gap before a section.
fn sidebar_screen_rows(rows: &[Row]) -> Vec<Option<usize>> {
    let mut lines = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        if matches!(row, Row::Section(_)) && idx > 0 {
            lines.push(None);
        }
        lines.push(Some(idx));
    }
    lines
}

fn render_sidebar_row(
    buf: &mut Buffer,
    area: Rect,
    row: &Row,
    is_selected: bool,
    state: &HubBrowserState,
    theme: &Theme,
) {
    let bg = if is_selected {
        theme.bg_highlight
    } else {
        theme.bg_dark
    };
    fill(buf, area, Style::default().bg(bg));
    let snapshot = &state.snapshot;
    let (marker, marker_style, name, right) = match row {
        Row::Section(title) => {
            let style = Style::default()
                .fg(theme.gray_bright)
                .bg(bg)
                .add_modifier(Modifier::BOLD);
            span_at(buf, area.x + 2, area.y, title, style, area.right());
            return;
        }
        Row::Channel(name) => (
            "# ".to_string(),
            Style::default().fg(theme.gray).bg(bg),
            name.clone(),
            String::new(),
        ),
        Row::Dm { peer, .. } => {
            let member = snapshot.meta.member(peer);
            let is_person = member.is_some_and(|m| m.kind == MemberKind::Person);
            let is_live = member.is_some_and(|m| m.active);
            let (dot, color) = match (is_person, is_live) {
                (true, _) => ("◆ ", theme.accent_plan),
                (false, true) => ("● ", theme.accent_success),
                (false, false) => ("○ ", theme.gray),
            };
            let label = match member {
                Some(m) if m.kind == MemberKind::Agent => m.label.clone(),
                Some(_) if *peer == state.person => "you".to_string(),
                Some(_) => "person".to_string(),
                None => "left".to_string(),
            };
            (
                dot.to_string(),
                Style::default().fg(color).bg(bg),
                peer.clone(),
                label,
            )
        }
    };
    let unread = row.conv().map(|conv| snapshot.unread(&conv)).unwrap_or(0);
    let is_dim = match row {
        Row::Dm { peer, .. } => snapshot.meta.member(peer).is_none_or(|m| !m.active),
        _ => false,
    };
    let mut name_style = Style::default().bg(bg).fg(if is_dim {
        theme.gray
    } else {
        theme.gray_bright
    });
    if unread > 0 || is_selected {
        name_style = name_style
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD);
    }
    let x = span_at(buf, area.x + 2, area.y, &marker, marker_style, area.right());
    let (right_text, right_style) = if unread > 0 {
        (
            format!(" {unread} "),
            Style::default()
                .fg(theme.bg_base)
                .bg(theme.accent_error)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (right, Style::default().fg(theme.gray).bg(bg))
    };
    let right_x = area.right().saturating_sub(right_text.width() as u16 + 1);
    span_at(buf, x, area.y, &name, name_style, right_x.saturating_sub(1));
    span_at(buf, right_x, area.y, &right_text, right_style, area.right());
}

fn render_main(buf: &mut Buffer, area: Rect, state: &mut HubBrowserState, theme: &Theme) {
    let inner = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(4),
        ..area
    };
    let Some(row) = state.selected_row() else {
        return;
    };
    render_conversation_header(buf, inner, &row, state, theme);
    let compose_y = inner.bottom().saturating_sub(COMPOSE_ROWS + 1);
    let feed = Rect {
        y: inner.y + 3,
        height: compose_y.saturating_sub(inner.y + 4),
        ..inner
    };
    render_feed(buf, feed, &row, state, theme);
    if let Some(notice) = &state.notice {
        let style = if notice.starts_with("Not posted") {
            Style::default().fg(theme.accent_error)
        } else {
            theme.muted()
        };
        span_at(
            buf,
            inner.x,
            compose_y.saturating_sub(1),
            notice,
            style,
            inner.right(),
        );
    }
    let compose = Rect {
        y: compose_y,
        height: COMPOSE_ROWS,
        ..inner
    };
    render_compose(buf, compose, &row, state, theme);
}

fn render_conversation_header(
    buf: &mut Buffer,
    area: Rect,
    row: &Row,
    state: &HubBrowserState,
    theme: &Theme,
) {
    let y = area.y + 1;
    let title_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD);
    let (title, detail) = match row {
        Row::Section(_) => (String::new(), String::new()),
        Row::Channel(name) => {
            let purpose = state
                .snapshot
                .meta
                .channels
                .iter()
                .find(|c| &c.name == name)
                .map(|c| c.purpose.clone())
                .unwrap_or_default();
            (format!("# {name}"), purpose)
        }
        Row::Dm { peer, .. } => {
            let member = state.snapshot.meta.member(peer);
            let status = match member {
                Some(m) if m.kind == MemberKind::Person => "person".to_string(),
                Some(m) if m.active => format!("{} · live", m.label),
                Some(m) => format!("{} · gone", m.label),
                None => "left the hub".to_string(),
            };
            let detail = if state.is_person_view() {
                status
            } else {
                format!("{status} · DM with @{}", state.perspective)
            };
            (format!("@{peer}"), detail)
        }
    };
    let x = span_at(buf, area.x, y, &title, title_style, area.right());
    if !detail.is_empty() {
        span_at(
            buf,
            x,
            y,
            &format!("   {detail}"),
            theme.muted(),
            area.right(),
        );
    }
    span_at(
        buf,
        area.x,
        y + 1,
        &"─".repeat(area.width as usize),
        Style::default().fg(theme.gray_dim),
        area.right(),
    );
}

fn render_feed(
    buf: &mut Buffer,
    area: Rect,
    row: &Row,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let meta = &state.snapshot.meta;
    let persons: BTreeSet<String> = meta
        .members
        .iter()
        .filter(|m| m.kind == MemberKind::Person)
        .map(|m| m.handle.clone())
        .chain(std::iter::once(state.person.clone()))
        .collect();
    let mut mentions: BTreeSet<String> = meta
        .members
        .iter()
        .map(|m| format!("@{}", m.handle))
        .collect();
    mentions.insert(format!("@{}", store::HERE_MENTION));
    mentions.extend(meta.channels.iter().map(|c| format!("#{}", c.name)));
    let names = FeedNames {
        persons: &persons,
        mentions: &mentions,
    };
    let today = Local::now().date_naive();
    let lines = feed_lines(
        &state.feed,
        area.width as usize,
        state.first_unread,
        &names,
        today,
    );
    if lines.is_empty() {
        let text = empty_feed_text(row, state);
        span_at(
            buf,
            area.x,
            area.bottom().saturating_sub(1),
            &text,
            theme.muted(),
            area.right(),
        );
        return;
    }
    let height = area.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    state.feed_scroll = state.feed_scroll.min(max_scroll);
    let end = lines.len() - state.feed_scroll;
    let start = end.saturating_sub(height);
    // A short feed sits at the bottom, over the post box, like a chat.
    let top = area.bottom() - (end - start) as u16;
    for (i, line) in lines[start..end].iter().enumerate() {
        let y = top + i as u16;
        let mut x = area.x;
        for (text, part) in line {
            x = span_at(buf, x, y, text, part_style(*part, theme), area.right());
        }
    }
    if state.feed_scroll > 0 {
        let hint = format!(" ↓ {} more lines ", state.feed_scroll);
        let x = area.right().saturating_sub(hint.width() as u16);
        let style = Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_highlight);
        span_at(buf, x, area.bottom() - 1, &hint, style, area.right());
    }
}

fn empty_feed_text(row: &Row, state: &HubBrowserState) -> String {
    match row {
        Row::Section(_) => String::new(),
        Row::Channel(name) => format!("This is the very beginning of #{name}."),
        Row::Dm { peer, .. } if state.is_person_view() => {
            format!("This is the very beginning of your DM with @{peer}.")
        }
        Row::Dm { peer, .. } => format!("@{} and @{peer} have no DMs yet.", state.perspective),
    }
}

fn part_style(part: Part, theme: &Theme) -> Style {
    let palette = [
        theme.accent_user,
        theme.accent_assistant,
        theme.accent_skill,
        theme.accent_feedback,
        theme.accent_verify,
        theme.running,
    ];
    match part {
        Part::Agent(slot) => Style::default()
            .fg(palette[slot % palette.len()])
            .add_modifier(Modifier::BOLD),
        Part::Person => Style::default()
            .fg(theme.accent_plan)
            .add_modifier(Modifier::BOLD),
        Part::Time => Style::default().fg(theme.gray),
        Part::Text => Style::default().fg(theme.text_primary),
        Part::Mention => Style::default()
            .fg(theme.accent_user)
            .bg(theme.bg_highlight),
        Part::Muted => Style::default().fg(theme.gray),
        Part::Rule => Style::default().fg(theme.gray_dim),
        Part::NewRule => Style::default().fg(theme.accent_error),
    }
}

fn render_compose(
    buf: &mut Buffer,
    area: Rect,
    row: &Row,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let border = if state.is_composing {
        theme.prompt_border_active
    } else if state.is_person_view() {
        theme.prompt_border
    } else {
        theme.gray_dim
    };
    let border_style = Style::default().fg(border);
    let inner_w = area.width.saturating_sub(2) as usize;
    span_at(
        buf,
        area.x,
        area.y,
        &format!("╭{}╮", "─".repeat(inner_w)),
        border_style,
        area.right(),
    );
    span_at(buf, area.x, area.y + 1, "│", border_style, area.right());
    span_at(
        buf,
        area.right() - 1,
        area.y + 1,
        "│",
        border_style,
        area.right(),
    );
    span_at(
        buf,
        area.x,
        area.y + 2,
        &format!("╰{}╯", "─".repeat(inner_w)),
        border_style,
        area.right(),
    );
    state.hits.compose = area;
    let text_w = inner_w.saturating_sub(2);
    let (text, style) = compose_text(row, state, text_w, theme);
    span_at(buf, area.x + 2, area.y + 1, &text, style, area.right() - 2);
}

fn compose_text(
    row: &Row,
    state: &HubBrowserState,
    width: usize,
    theme: &Theme,
) -> (String, Style) {
    if !state.is_person_view() {
        let text = format!(
            "Viewing as @{} · read only · press p to switch back to you",
            state.perspective
        );
        return (text, theme.muted().add_modifier(Modifier::ITALIC));
    }
    let to = state
        .post_target()
        .or_else(|| post_target(row))
        .unwrap_or_default();
    if state.is_composing {
        let text = format!("{}█", state.draft);
        return (
            tail_to_width(&text, width),
            Style::default().fg(theme.text_primary),
        );
    }
    if !state.draft.is_empty() {
        let text = format!("Draft to {to}: {}", state.draft);
        return (tail_to_width(&text, width), theme.muted());
    }
    (format!("Message {to}"), theme.muted())
}

/// The end of `text` that fits in `width` columns.
fn tail_to_width(text: &str, width: usize) -> String {
    let mut out: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.into_iter().rev().collect()
}

fn render_footer(buf: &mut Buffer, bar: Rect, state: &HubBrowserState, theme: &Theme) {
    let bar_bg = theme.bg_dark;
    fill(buf, bar, Style::default().bg(bar_bg));
    let keys: &[(&str, &str)] = if state.picker.is_some() {
        &[("↑↓", "choose"), ("Enter", "view as"), ("Esc", "cancel")]
    } else if state.is_composing {
        &[("Enter", "send"), ("Esc", "stop writing")]
    } else if state.is_person_view() {
        &[
            ("↑↓", "conversations"),
            ("Tab", "write"),
            ("p", "perspective"),
            ("PgUp/PgDn", "scroll"),
            ("Esc", "close"),
        ]
    } else {
        &[
            ("↑↓", "conversations"),
            ("p", "perspective"),
            ("PgUp/PgDn", "scroll"),
            ("Esc", "close"),
        ]
    };
    let mut x = bar.x + 2;
    for (key, label) in keys {
        x = span_at(
            buf,
            x,
            bar.y,
            key,
            Style::default()
                .fg(theme.gray_bright)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
            bar.right(),
        );
        x = span_at(
            buf,
            x,
            bar.y,
            &format!(" {label}   "),
            Style::default().fg(theme.gray).bg(bar_bg),
            bar.right(),
        );
    }
}

fn render_picker(buf: &mut Buffer, area: Rect, state: &mut HubBrowserState, theme: &Theme) {
    let members = state.picker_members();
    let labels: Vec<(String, String)> = members
        .iter()
        .map(|handle| picker_label(handle, state))
        .collect();
    let content_w = labels
        .iter()
        .map(|(name, detail)| name.width() + detail.width() + 6)
        .max()
        .unwrap_or(10)
        .max(24) as u16;
    let width = (content_w + 2).min(area.width);
    let title_rows = 2;
    let height = (members.len() as u16 + title_rows + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: state.hits.chip.right().saturating_sub(width).max(area.x),
        y: area.y + 1,
        width,
        height,
    };
    Clear.render(rect, buf);
    fill(buf, rect, Style::default().bg(theme.bg_base));
    let border_style = Style::default()
        .fg(theme.prompt_border_active)
        .bg(theme.bg_base);
    let inner_w = rect.width.saturating_sub(2) as usize;
    span_at(
        buf,
        rect.x,
        rect.y,
        &format!("╭{}╮", "─".repeat(inner_w)),
        border_style,
        rect.right(),
    );
    for y in rect.y + 1..rect.bottom() - 1 {
        span_at(buf, rect.x, y, "│", border_style, rect.right());
        span_at(buf, rect.right() - 1, y, "│", border_style, rect.right());
    }
    span_at(
        buf,
        rect.x,
        rect.bottom() - 1,
        &format!("╰{}╯", "─".repeat(inner_w)),
        border_style,
        rect.right(),
    );
    let inner_x = rect.x + 2;
    let inner_right = rect.right() - 2;
    span_at(
        buf,
        inner_x,
        rect.y + 1,
        "View the hub as",
        theme.muted().bg(theme.bg_base),
        inner_right,
    );
    let visible = rect.height.saturating_sub(title_rows + 2) as usize;
    let highlighted = state.picker.unwrap_or(0);
    let first = (highlighted + 1).saturating_sub(visible);
    for (offset, idx) in (first..members.len()).take(visible).enumerate() {
        let y = rect.y + 1 + title_rows + offset as u16;
        let is_highlighted = idx == highlighted;
        let bg = if is_highlighted {
            theme.bg_highlight
        } else {
            theme.bg_base
        };
        let row_rect = Rect {
            x: rect.x + 1,
            y,
            width: rect.width.saturating_sub(2),
            height: 1,
        };
        fill(buf, row_rect, Style::default().bg(bg));
        let check = if members[idx] == state.perspective {
            "✓ "
        } else {
            "  "
        };
        let (name, detail) = &labels[idx];
        let mut x = span_at(
            buf,
            inner_x,
            y,
            check,
            Style::default().fg(theme.accent_success).bg(bg),
            inner_right,
        );
        let mut name_style = Style::default().fg(theme.text_primary).bg(bg);
        if is_highlighted {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        x = span_at(buf, x, y, name, name_style, inner_right);
        span_at(
            buf,
            x,
            y,
            &format!("  {detail}"),
            Style::default().fg(theme.gray).bg(bg),
            inner_right,
        );
        state.hits.picker.push((row_rect, members[idx].clone()));
    }
}

fn picker_label(handle: &str, state: &HubBrowserState) -> (String, String) {
    if handle == state.person {
        return ("You".to_string(), format!("@{handle}"));
    }
    let detail = match state.snapshot.meta.member(handle) {
        Some(m) if m.kind == MemberKind::Person => "person".to_string(),
        Some(m) if m.active => format!("{} · live", m.label),
        Some(m) => format!("{} · gone", m.label),
        None => String::new(),
    };
    (format!("@{handle}"), detail)
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    buf.set_style(area, style);
}

/// Draw `text` at (x, y), clipped at `max_x`. Returns the x after the text.
/// A style with no background takes the background of the cell under it.
fn span_at(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max_x: u16) -> u16 {
    if max_x <= x || text.is_empty() {
        return x;
    }
    let style = match style.bg {
        Some(_) => style,
        None => match buf.cell((x, y)).map(|cell| cell.bg) {
            Some(bg) if bg != Color::Reset => style.bg(bg),
            _ => style,
        },
    };
    let width = (text.width() as u16).min(max_x - x);
    buf.set_span_safe(
        x,
        y,
        &ratatui::text::Span::styled(text.to_string(), style),
        max_x - x,
    );
    x + width
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn message(from: &str, conv: String, text: &str, ts_unix_ms: u64) -> store::Message {
        store::Message {
            id: format!("{from}-{ts_unix_ms}-{text}"),
            conv,
            from: from.to_string(),
            text: text.to_string(),
            reply_to: None,
            mentions: Vec::new(),
            images: Vec::new(),
            ts_unix_ms,
        }
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
            hub.append(&message(from, conv, text, store::now_ms()))
                .unwrap();
        }
        (root, hub)
    }

    fn open(hub: store::Hub) -> HubBrowserState {
        HubBrowserState::open(hub, "me".to_string(), None).unwrap()
    }

    fn dm(peer: &str, perspective: &str) -> Row {
        Row::Dm {
            peer: peer.to_string(),
            conv: store::dm_conv(perspective, peer),
        }
    }

    #[test]
    fn person_view_lists_channels_then_every_member_as_a_dm() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        assert_eq!(
            state.rows(),
            vec![
                Row::Section("Channels"),
                Row::Channel("general".to_string()),
                Row::Section("Direct messages"),
                dm("front", "me"),
                dm("back", "me"),
            ]
        );
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
        assert_eq!(state.feed.len(), 1);
        assert_eq!(state.post_target().as_deref(), Some("#general"));

        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
        assert_eq!(state.feed.len(), 1);
        assert_eq!(state.post_target().as_deref(), Some("@front"));
        state.handle_key(&key(KeyCode::Down));
        assert!(
            state.feed.is_empty(),
            "a DM with no messages is still listed"
        );
        assert_eq!(state.post_target().as_deref(), Some("@back"));
    }

    #[test]
    fn agent_perspective_shows_its_dms_read_only() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('p')));
        assert_eq!(state.picker_members(), vec!["me", "front", "back"]);
        state.handle_key(&key(KeyCode::Down));
        state.handle_key(&key(KeyCode::Enter));
        assert_eq!(state.perspective, "front");
        assert!(state.picker.is_none());
        assert_eq!(
            state.rows(),
            vec![
                Row::Section("Channels"),
                Row::Channel("general".to_string()),
                Row::Section("Direct messages"),
                dm("back", "front"),
                dm("me", "front"),
            ]
        );
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
        assert_eq!(
            state.post_target(),
            None,
            "another member's view is read only"
        );
        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(dm("back", "front")));
        assert_eq!(state.feed.len(), 2);
        assert_eq!(state.handle_key(&key(KeyCode::Tab)), HubKeyOutcome::Changed);
        assert!(!state.is_composing);

        state.set_perspective("back").unwrap();
        assert_eq!(state.rows().len(), 4, "back has one DM");
        state.set_perspective("me").unwrap();
        assert_eq!(state.post_target().as_deref(), Some("#general"));
    }

    #[test]
    fn only_the_person_read_cursor_moves() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub.clone());
        assert_eq!(hub.cursors("me").unwrap().read.get("#general"), Some(&1));
        state.set_perspective("front").unwrap();
        assert_eq!(state.snapshot.unread("#general"), 1);
        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.first_unread, Some(0));
        assert!(hub.cursors("front").unwrap().read.is_empty());
        assert!(hub.cursors("back").unwrap().read.is_empty());
    }

    #[test]
    fn unread_marker_points_at_the_first_unread_message() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub.clone());
        assert_eq!(state.first_unread, None, "your own message is not new");
        hub.append(&message(
            "front",
            "#general".to_string(),
            "new one",
            store::now_ms(),
        ))
        .unwrap();
        state.handle_key(&key(KeyCode::Down));
        state.handle_key(&key(KeyCode::Up));
        assert_eq!(state.feed.len(), 2);
        assert_eq!(state.first_unread, Some(1));
        state.reload().unwrap();
        assert_eq!(state.first_unread, Some(1), "a reload keeps the marker");
        state.handle_key(&key(KeyCode::Down));
        state.handle_key(&key(KeyCode::Up));
        assert_eq!(state.first_unread, None, "the person has read everything");
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
        for selected in [Row::Channel("general".to_string()), dm("front", "me")] {
            let (_root, hub) = hub_with_dms();
            let mut state = open(hub);
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
    fn reload_does_not_redirect_a_draft_when_its_row_disappears() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("for front only");
        state
            .hub
            .update_meta(|meta| meta.members.retain(|member| member.handle != "front"))
            .unwrap();
        // The DM file keeps the row, so remove its messages too.
        std::fs::remove_dir_all(state.hub.dir().join("dms")).unwrap();

        state.reload().unwrap();

        assert_ne!(state.selected_row(), Some(dm("front", "me")));
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
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("hello");
        state.handle_key(&key(KeyCode::Esc));
        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
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
    fn mouse_clicks_select_rows_and_switch_perspective() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let (y, _) = *state
            .hits
            .rows
            .iter()
            .find(|(_, idx)| state.rows()[*idx] == dm("front", "me"))
            .unwrap();
        let down = MouseEventKind::Down(MouseButton::Left);
        state.handle_mouse(down, 3, y);
        assert_eq!(state.selected_row(), Some(dm("front", "me")));

        let chip = state.hits.chip;
        state.handle_mouse(down, chip.x + 1, chip.y);
        assert!(state.picker.is_some());
        render_hub_browser(&mut buf, area, &mut state);
        let (rect, _) = state
            .hits
            .picker
            .iter()
            .find(|(_, handle)| handle == "back")
            .unwrap()
            .clone();
        state.handle_mouse(down, rect.x + 1, rect.y);
        assert_eq!(state.perspective, "back");
        assert!(state.picker.is_none());
    }

    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_shows_the_slack_layout_and_the_perspective_chip() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 100, 24);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let text = buffer_text(&buf);
        for needle in [
            "◆ demo",
            "Viewing as  ◉ You ▾",
            "Channels",
            "# general",
            "Direct messages",
            "● front",
            "@me",
            "hello",
            "Message #general",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        state.set_perspective("front").unwrap();
        render_hub_browser(&mut buf, area, &mut state);
        let text = buffer_text(&buf);
        assert!(text.contains("◉ @front ▾"), "{text}");
        assert!(text.contains("read only"), "{text}");
    }

    #[test]
    fn feed_lines_group_messages_and_mark_days_and_unread() {
        let persons = BTreeSet::from(["me".to_string()]);
        let mentions = BTreeSet::from(["@front".to_string(), "#general".to_string()]);
        let names = FeedNames {
            persons: &persons,
            mentions: &mentions,
        };
        let day_ms = 24 * 60 * 60 * 1000;
        let base = 10 * day_ms;
        let messages = vec![
            message("front", "#general".into(), "one", base),
            message("front", "#general".into(), "two", base + 60_000),
            message(
                "me",
                "#general".into(),
                "ping @front, see #general",
                base + 120_000,
            ),
            message("me", "#general".into(), "next day", base + day_ms),
        ];
        let today = local_time(base + day_ms).date_naive();
        let lines = feed_lines(&messages, 40, Some(2), &names, today);
        let plain: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        assert!(plain[0].contains(" Yesterday "), "{plain:?}");
        assert!(plain[1].starts_with("@front  "), "{plain:?}");
        assert_eq!(plain[2], "  one");
        assert_eq!(
            plain[3], "  two",
            "a close message from the same sender shares a header"
        );
        assert!(plain[4].ends_with(" new"), "{plain:?}");
        assert!(plain[5].starts_with("@me  "));
        assert!(lines[5][0].1 == Part::Person);
        assert!(lines[6].contains(&("@front".to_string(), Part::Mention)));
        assert!(lines[6].contains(&("#general".to_string(), Part::Mention)));
        assert!(plain.iter().any(|line| line.contains(" Today ")));
        for line in &plain {
            assert!(line.width() <= 40, "{line:?}");
        }
        assert!(feed_lines(&messages, 3, None, &names, today).is_empty());

        let lines = feed_lines(&messages, 40, Some(0), &names, today);
        let first: String = lines[0].iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            first.contains(" Yesterday ") && first.ends_with(" new"),
            "{first}"
        );
        assert_eq!(first.width(), 40);
        assert!(lines[1][0].0.starts_with("@front"), "one rule marks both");
    }

    #[test]
    fn feed_lines_keep_bodies_visible_with_long_sender_names() {
        let from = "a".repeat(64);
        let msg = message(&from, "#general".into(), "one two three four five six", 0);
        let persons = BTreeSet::new();
        let mentions = BTreeSet::new();
        let names = FeedNames {
            persons: &persons,
            mentions: &mentions,
        };
        let today = local_time(0).date_naive();
        for width in [8, 20, 44] {
            let lines = feed_lines(std::slice::from_ref(&msg), width, None, &names, today);
            let body: String = lines
                .iter()
                .flatten()
                .filter(|(_, part)| *part == Part::Text)
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                body.split_whitespace().collect::<String>(),
                msg.text.split_whitespace().collect::<String>(),
            );
            for line in &lines {
                assert!(line.iter().map(|(t, _)| t.width()).sum::<usize>() <= width);
            }
        }
    }
}
