//! Hub browser: a full-screen, Slack-style view of one agent hub (`/hub open`).
//!
//! The view shows the hub from the perspective of one member. The left column
//! has the channels, then the direct messages of that member. The main pane
//! shows the selected conversation as a chat thread, with a post box at the
//! bottom. A chip at the top right selects the perspective: the person (you)
//! by default, or any other member. Only the person can post. Only the
//! person's read cursor moves when the view shows a conversation.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;
use xai_grok_tools::implementations::grok_build::agent_hub::{self, MemberKind, store};

use crate::input::key::KeyShortcut;
use crate::render::SafeBuf;
use crate::render::osc8::{LinkOverlay, LinkTarget, scan_lines_for_url_overlays};
use crate::render::scrollbar::{ScrollbarClickResult, render_scrollbar, scrollbar_click_to_offset};
use crate::scrollback::types::{col_past_grapheme, grapheme_cells_at, slice_display_cols};
use crate::theme::Theme;
use crate::views::hub_composer::{CompletionNames, ComposerKey, HubComposer};
use crate::views::shortcuts_bar::{HintItem, ShortcutsBar};

/// Messages that one feed shows.
const FEED_LIMIT: usize = 300;
/// Widest the sidebar gets.
const SIDEBAR_MAX_WIDTH: u16 = 30;
/// Messages from one sender closer than this share one header.
const GROUP_GAP_MS: u64 = 5 * 60 * 1000;
/// Feed lines that PgUp and PgDn scroll.
const PAGE_LINES: usize = 10;
/// Lines for one wheel notch when an event reaches `handle_mouse` directly.
const WHEEL_LINES: i32 = 3;
/// Columns of the message body indent under a sender header.
const BODY_INDENT: usize = 2;
/// Title of the sidebar section that has the DM filter toggle.
const DM_SECTION: &str = "Direct messages";

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

/// Which members the "Direct messages" section lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DmFilter {
    #[default]
    All,
    /// Live agents, persons, unread DMs, and the selected DM.
    Active,
}

impl DmFilter {
    fn toggled(self) -> Self {
        match self {
            DmFilter::All => DmFilter::Active,
            DmFilter::Active => DmFilter::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DmFilter::All => " all ",
            DmFilter::Active => " active ",
        }
    }
}

/// One end of a feed text selection: a feed line and a display column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FeedPoint {
    line: usize,
    col: u16,
}

/// A text selection in the feed, made with a left drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FeedSelection {
    anchor: FeedPoint,
    head: FeedPoint,
    /// True while the left button is down.
    is_dragging: bool,
}

impl FeedSelection {
    /// The endpoints in reading order. Both columns are inclusive.
    fn ordered(&self) -> (FeedPoint, FeedPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The selected display columns of `line`, as a half-open range.
    fn columns_on(&self, line: usize) -> Option<Range<u16>> {
        let (start, end) = self.ordered();
        if line < start.line || line > end.line {
            return None;
        }
        let from = if line == start.line { start.col } else { 0 };
        let to = if line == end.line {
            end.col.saturating_add(1)
        } else {
            u16::MAX
        };
        Some(from..to)
    }
}

/// What an input event asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubKeyOutcome {
    Close,
    Changed,
    /// Nothing visible changed, so the caller can skip a redraw.
    Unchanged,
    Post {
        to: String,
        text: String,
    },
    /// Open a URL or a file from the feed.
    OpenLink(LinkTarget),
}

/// What a clickable feed span points to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpanTarget {
    /// A member handle, without `@`.
    Member(String),
    /// A channel name, without `#`.
    Channel(String),
    Link(LinkTarget),
}

/// One clickable span of the feed in the last frame.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FeedSpanHit {
    row: u16,
    cols: Range<u16>,
    target: SpanTarget,
}

/// The element under the mouse pointer. A change of this value needs a redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverKey {
    /// A feed span, by its screen row and first column.
    Span(u16, u16),
    /// A sidebar row, by its screen row.
    SidebarRow(u16),
    Chip,
    DmToggle,
    Compose,
    /// A footer hint, by its index.
    Hint(usize),
}

/// Screen areas of the last frame, for mouse hits.
#[derive(Debug, Clone, Default)]
struct HitAreas {
    sidebar: Rect,
    /// The thread pane right of the sidebar, with its scrollbar column.
    main: Rect,
    /// Screen row and identity of each visible sidebar row.
    rows: Vec<(u16, Row)>,
    chip: Rect,
    picker: Vec<(Rect, String)>,
    compose: Rect,
    /// The feed scrollbar column. It is empty when the feed fits.
    feed_scrollbar: Rect,
    /// Lines of the feed in the last frame.
    feed_line_count: usize,
    /// The feed text area, without the scrollbar.
    feed: Rect,
    /// Screen row of the first drawn feed line, and its index in `feed_text`.
    feed_top: u16,
    feed_first_line: usize,
    /// Plain text of every feed line, for drag selection and copy.
    feed_text: Vec<String>,
    /// The all/active toggle on the "Direct messages" section row.
    dm_toggle: Rect,
    /// Clickable spans of the visible feed lines.
    feed_spans: Vec<FeedSpanHit>,
    /// Screen area and key of each footer hint. A hint with no single key
    /// is not clickable.
    footer_hints: Vec<(Rect, Option<KeyShortcut>)>,
}

/// Purpose: Hold the hub browser state for one perspective.
/// Note: A row keeps its identity when the sidebar filter changes its index.
#[derive(Debug)]
pub struct HubBrowserState {
    pub hub: store::Hub,
    /// The name of the person at the keyboard, before it becomes a handle.
    person_name: String,
    /// The handle of the person at the keyboard. A reload checks it again,
    /// because the person does not reserve it until the first post.
    pub person: String,
    /// Read positions of a person who is not a member yet. A cursor file for a
    /// free handle would pass to an agent that joins with that handle later.
    unsaved_read: BTreeMap<String, usize>,
    /// The member whose view the browser shows.
    pub perspective: String,
    pub snapshot: HubSnapshot,
    pub feed: Vec<store::Message>,
    /// Feed index of the first message that the perspective member had not read.
    pub first_unread: Option<usize>,
    selected: Option<Row>,
    pub composer: HubComposer,
    pub is_composing: bool,
    /// The terminal cursor cell of the post box in the last frame.
    pub cursor: Option<(u16, u16)>,
    /// A draft keeps its destination even if the selected row disappears.
    compose_target: Option<String>,
    /// Feed lines scrolled up from the newest message.
    pub feed_scroll: usize,
    /// Result of the last post, shown over the post box.
    pub notice: Option<String>,
    /// The highlighted index of the open perspective picker.
    pub picker: Option<usize>,
    /// The first member row that the picker shows. A hover keeps it, so the
    /// row under the pointer stays there until the click.
    picker_top: usize,
    /// A left press on the feed scrollbar holds it until the button is up.
    is_dragging_scrollbar: bool,
    /// Which members the DM section lists.
    pub dm_filter: DmFilter,
    /// The feed text selection. It stays visible until the next click.
    feed_selection: Option<FeedSelection>,
    /// Text of the last finished drag. The caller takes it and copies it.
    pub copy_text: Option<String>,
    /// The mouse pointer cell from the last mouse event.
    pointer: Option<(u16, u16)>,
    /// The element under the pointer in the last frame.
    hover: Option<HoverKey>,
    hits: HitAreas,
}

impl HubBrowserState {
    /// Purpose: Open the browser on a hub, as the person.
    /// Input: The hub, the person name, and an optional `#channel` to select.
    /// Output: The state, or an error when the hub files cannot be read.
    pub fn open(hub: store::Hub, person_name: &str, channel: Option<&str>) -> Result<Self> {
        let person = agent_hub::person_handle_in(&hub.meta()?, person_name);
        let snapshot = load_snapshot(&hub, &person, &person, &BTreeMap::new())?;
        let mut state = Self {
            hub,
            person_name: person_name.to_string(),
            perspective: person.clone(),
            person,
            unsaved_read: BTreeMap::new(),
            snapshot,
            feed: Vec::new(),
            first_unread: None,
            selected: None,
            composer: HubComposer::default(),
            is_composing: false,
            cursor: None,
            compose_target: None,
            feed_scroll: 0,
            notice: None,
            picker: None,
            picker_top: 0,
            is_dragging_scrollbar: false,
            dm_filter: DmFilter::default(),
            feed_selection: None,
            copy_text: None,
            pointer: None,
            hover: None,
            hits: HitAreas::default(),
        };
        let rows = state.rows();
        let target = channel.and_then(store::normalize_channel).map(Row::Channel);
        let idx = target
            .and_then(|target| rows.iter().position(|row| *row == target))
            .unwrap_or_else(|| clamp_selection(&rows, 1));
        state.select(rows.get(idx).cloned());
        Ok(state)
    }

    pub fn rows(&self) -> Vec<Row> {
        sidebar_rows_with_selection(
            &self.snapshot,
            &self.perspective,
            self.dm_filter,
            self.selected.as_ref(),
        )
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.selected.clone()
    }

    fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        rows.iter()
            .position(|row| Some(row) == self.selected.as_ref())
    }

    pub fn is_person_view(&self) -> bool {
        self.perspective == self.person
    }

    /// Purpose: Read the hub again and keep the selected row when it still exists.
    /// Input: The browser state and the hub files.
    /// Output: Ok, or the read error. A row that is gone selects a valid fallback.
    pub fn reload(&mut self) -> Result<()> {
        let previous_index = self.selected_index(&self.rows()).unwrap_or(0);
        self.reload_snapshot()?;
        self.reselect(previous_index)
    }

    /// Purpose: Switch the DM section between all members and active members.
    /// Input: The browser state.
    /// Output: Ok, or the feed read error. The selected row stays when it is listed.
    pub fn toggle_dm_filter(&mut self) -> Result<()> {
        let previous_index = self.selected_index(&self.rows()).unwrap_or(0);
        self.dm_filter = self.dm_filter.toggled();
        self.reselect(previous_index)
    }

    /// Reload the selected row if it still exists, or select a valid fallback.
    fn reselect(&mut self, previous_index: usize) -> Result<()> {
        let rows = self.rows();
        if self.selected_index(&rows).is_some() {
            return self.load_feed(false);
        }
        let fallback = clamp_selection(&rows, previous_index);
        self.set_selected(rows.get(fallback).cloned())
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
                let posted = self.composer.text().trim().to_string();
                self.composer.finish_send(&posted);
                self.compose_target = None;
                format!("Posted to {to} as @{}.", self.person)
            }
            Err(error) => format!("Not posted: {error}"),
        });
    }

    /// Purpose: Show the hub as another member sees it.
    /// Input: The member handle.
    /// Output: The new view. The selected conversation stays when it exists.
    pub fn set_perspective(&mut self, handle: &str) -> Result<()> {
        let kept_conv = self.selected_row().and_then(|row| row.conv());
        self.show_perspective(handle, kept_conv)
    }

    /// Purpose: Show the hub as another member sees it, at one conversation.
    /// Input: The member handle, and the conversation to select when that
    ///     member has it.
    /// Output: The new view. Without that conversation, the top row is selected.
    fn show_perspective(&mut self, handle: &str, kept_conv: Option<String>) -> Result<()> {
        self.picker = None;
        if handle == self.perspective {
            return Ok(());
        }
        self.perspective = handle.to_string();
        self.stop_compose();
        self.notice = None;
        self.reload_snapshot()?;
        // Keep the same conversation if this member has it. A row index means
        // nothing in another sidebar, so the fallback is the top. Pick the row
        // before the feed loads, so no other conversation is marked read.
        let rows = self.rows();
        let index = rows
            .iter()
            .position(|row| kept_conv.is_some() && row.conv() == kept_conv)
            .unwrap_or_else(|| clamp_selection(&rows, 0));
        // The unread marker belongs to the member whose view this is.
        self.set_selected(rows.get(index).cloned())
    }

    /// Purpose: Read the hub files again, with no change to the selection.
    /// Input: The browser state and the hub files.
    /// Output: Ok, or the read error.
    /// Note: If an agent took the person handle, the person gets the next free
    ///     handle, so the browser never writes the read cursor of an agent.
    fn reload_snapshot(&mut self) -> Result<()> {
        let meta = self.hub.meta()?;
        let person = agent_hub::person_handle_in(&meta, &self.person_name);
        if person != self.person {
            if self.is_person_view() {
                self.perspective = person.clone();
            }
            self.person = person;
        }
        if is_person_member(&meta, &self.person) {
            // The person posted since the last read, so the handle is safe to save.
            for (conv, count) in std::mem::take(&mut self.unsaved_read) {
                agent_hub::mark_read(&self.hub, &self.person, &conv, count)?;
            }
        }
        let read_floor = if self.is_person_view() {
            self.unsaved_read.clone()
        } else {
            BTreeMap::new()
        };
        self.snapshot = load_snapshot(&self.hub, &self.perspective, &self.person, &read_floor)?;
        Ok(())
    }

    /// The person name that a post uses to find or reserve the person handle.
    pub fn person_name(&self) -> &str {
        &self.person_name
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
            KeyCode::Char('a') => {
                if let Err(error) = self.toggle_dm_filter() {
                    tracing::warn!(%error, "hub browser: feed load failed");
                }
            }
            KeyCode::Tab | KeyCode::Enter | KeyCode::Char('i') => self.start_compose(),
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.is_composing {
            let names = self.completion_names();
            self.composer.paste(text, &names);
        }
    }

    /// The handles and channels that the post box completes.
    fn completion_names(&self) -> CompletionNames {
        let meta = &self.snapshot.meta;
        let mut handles: Vec<String> = meta
            .members
            .iter()
            .filter(|member| member.handle != self.person)
            .map(|member| member.handle.clone())
            .collect();
        handles.push(store::HERE_MENTION.to_string());
        CompletionNames {
            handles,
            channels: meta.channels.iter().map(|c| c.name.clone()).collect(),
        }
    }

    /// Purpose: Apply one mouse event to the browser.
    /// Input: The event kind and the screen position.
    /// Output: What the caller must do next.
    /// Note: The hit areas come from the last frame.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> HubKeyOutcome {
        self.pointer = Some((col, row));
        if self.is_composing && self.composer.popup_contains(col, row) {
            return match self.composer.handle_mouse(kind, col, row) {
                true => HubKeyOutcome::Changed,
                false => HubKeyOutcome::Unchanged,
            };
        }
        match kind {
            MouseEventKind::Moved => {
                // Leaving the popup can clear its hover row.
                let is_popup_changed = self.composer.handle_mouse(kind, col, row);
                let outcome = self.update_hover(col, row);
                if is_popup_changed {
                    return HubKeyOutcome::Changed;
                }
                return outcome;
            }
            MouseEventKind::ScrollUp => self.handle_scroll(-WHEEL_LINES, col, row),
            MouseEventKind::ScrollDown => self.handle_scroll(WHEEL_LINES, col, row),
            MouseEventKind::Down(MouseButton::Left)
                if self.picker.is_none() && contains(self.hits.feed_scrollbar, col, row) =>
            {
                self.is_dragging_scrollbar = true;
                self.drag_feed_scrollbar(row);
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.picker.is_none() && contains(self.hits.feed, col, row) =>
            {
                self.is_dragging_scrollbar = false;
                let point = self.feed_point_at(col, row);
                self.feed_selection = Some(FeedSelection {
                    anchor: point,
                    head: point,
                    is_dragging: true,
                });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.is_dragging_scrollbar = false;
                self.feed_selection = None;
                return self.click(col, row);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.is_dragging_scrollbar => {
                self.drag_feed_scrollbar(row);
            }
            MouseEventKind::Drag(MouseButton::Left) => self.drag_feed_selection(col, row),
            MouseEventKind::Up(MouseButton::Left) => {
                self.is_dragging_scrollbar = false;
                let is_plain_click = self
                    .feed_selection
                    .is_some_and(|s| s.is_dragging && s.anchor == s.head);
                self.finish_feed_selection();
                if is_plain_click && let Some(target) = self.feed_span_at(col, row) {
                    return self.activate(target);
                }
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    /// Purpose: Track the element under the pointer.
    /// Input: The pointer position.
    /// Output: `Changed` when the hovered element is another one, so the
    ///     hover style moves. Otherwise `Unchanged`.
    fn update_hover(&mut self, col: u16, row: u16) -> HubKeyOutcome {
        if let Some(idx) = self.picker_row_at(col, row) {
            if self.picker == Some(idx) {
                return HubKeyOutcome::Unchanged;
            }
            self.picker = Some(idx);
            return HubKeyOutcome::Changed;
        }
        let hover = self.hover_key_at(col, row);
        if hover == self.hover {
            return HubKeyOutcome::Unchanged;
        }
        self.hover = hover;
        HubKeyOutcome::Changed
    }

    /// The element at a screen cell, from the hit areas of the last frame.
    fn hover_key_at(&self, col: u16, row: u16) -> Option<HoverKey> {
        let hits = &self.hits;
        if self.picker.is_some() {
            return None;
        }
        if let Some(idx) = hits
            .footer_hints
            .iter()
            .position(|(rect, key)| key.is_some() && contains(*rect, col, row))
        {
            return Some(HoverKey::Hint(idx));
        }
        if contains(hits.chip, col, row) {
            return Some(HoverKey::Chip);
        }
        if contains(hits.dm_toggle, col, row) {
            return Some(HoverKey::DmToggle);
        }
        if self.is_person_view() && contains(hits.compose, col, row) {
            return Some(HoverKey::Compose);
        }
        if contains(hits.sidebar, col, row) {
            let is_row = hits
                .rows
                .iter()
                .any(|(y, hit)| *y == row && hit.is_selectable());
            return is_row.then_some(HoverKey::SidebarRow(row));
        }
        hits.feed_spans
            .iter()
            .find(|span| span.row == row && span.cols.contains(&col))
            .map(|span| HoverKey::Span(span.row, span.cols.start))
    }

    /// The target of the clickable feed span at a screen cell.
    fn feed_span_at(&self, col: u16, row: u16) -> Option<SpanTarget> {
        self.hits
            .feed_spans
            .iter()
            .find(|span| span.row == row && span.cols.contains(&col))
            .map(|span| span.target.clone())
    }

    fn picker_row_at(&self, col: u16, row: u16) -> Option<usize> {
        self.picker?;
        let (_, handle) = self
            .hits
            .picker
            .iter()
            .find(|(rect, _)| contains(*rect, col, row))?;
        self.picker_members().iter().position(|h| h == handle)
    }

    /// Purpose: Follow a clicked feed span, like a link in a chat app.
    /// Input: The span target.
    /// Output: The next browser action. A member opens the DM of the person
    ///     with that member. A channel opens the channel. A link goes to the caller.
    fn activate(&mut self, target: SpanTarget) -> HubKeyOutcome {
        match target {
            SpanTarget::Member(handle) => self.open_dm_with(&handle),
            SpanTarget::Channel(name) => {
                self.stop_compose();
                self.select(Some(Row::Channel(name)));
            }
            SpanTarget::Link(link) => return HubKeyOutcome::OpenLink(link),
        }
        HubKeyOutcome::Changed
    }

    /// Purpose: Show the DM of the person with a member.
    /// Input: The member handle.
    /// Output: None. The view changes to the person when it shows another
    ///     member. The own handle and unknown handles change nothing.
    pub fn open_dm_with(&mut self, handle: &str) {
        if handle == self.person || self.snapshot.meta.member(handle).is_none() {
            return;
        }
        if !self.is_person_view() {
            // Select the DM before the feed loads, so the switch does not
            // mark another conversation read.
            let person = self.person.clone();
            let conv = store::dm_conv(&person, handle);
            if let Err(error) = self.show_perspective(&person, Some(conv)) {
                self.notice = Some(format!("Could not load @{person}: {error}"));
            }
        }
        self.stop_compose();
        let row = Row::Dm {
            peer: handle.to_string(),
            conv: store::dm_conv(&self.person, handle),
        };
        if self.selected.as_ref() != Some(&row)
            && self.snapshot.dm_peers.iter().any(|peer| peer == handle)
        {
            self.select(Some(row));
        }
    }

    /// Move the head of an open feed selection to the pointer. A pointer above
    /// or below the feed scrolls the feed one line, like a terminal selection.
    fn drag_feed_selection(&mut self, col: u16, row: u16) {
        let Some(selection) = self.feed_selection else {
            return;
        };
        if !selection.is_dragging {
            return;
        }
        let feed = self.hits.feed;
        if row < feed.y {
            self.feed_scroll = self.feed_scroll.saturating_add(1);
        } else if row >= feed.bottom() {
            self.feed_scroll = self.feed_scroll.saturating_sub(1);
        }
        let head = self.feed_point_at(col, row);
        self.feed_selection = Some(FeedSelection { head, ..selection });
    }

    /// Purpose: End a feed drag and keep its text for the caller to copy.
    /// Input: The browser state.
    /// Output: None. A drag that covers text sets `copy_text`. A plain click
    ///     clears the selection.
    fn finish_feed_selection(&mut self) {
        let Some(selection) = self.feed_selection else {
            return;
        };
        if !selection.is_dragging {
            return;
        }
        if selection.anchor == selection.head {
            self.feed_selection = None;
            return;
        }
        self.feed_selection = Some(FeedSelection {
            is_dragging: false,
            ..selection
        });
        let text = selected_feed_text(&self.hits.feed_text, &selection);
        if !text.trim().is_empty() {
            self.copy_text = Some(text);
        }
    }

    /// Purpose: Map a screen cell to a feed line and column from the last frame.
    /// Input: The screen position. It can be outside the feed area.
    /// Output: The nearest feed point. Rows above the first line map to it,
    ///     and rows below the last line map to the end of the last line.
    fn feed_point_at(&self, col: u16, row: u16) -> FeedPoint {
        let hits = &self.hits;
        let last_line = hits.feed_text.len().saturating_sub(1);
        let col = col.saturating_sub(hits.feed.x);
        let col = col.min(hits.feed.width.saturating_sub(1));
        if row < hits.feed_top {
            let line = hits.feed_first_line.min(last_line);
            return FeedPoint { line, col: 0 };
        }
        let line = hits.feed_first_line + (row - hits.feed_top) as usize;
        if line > last_line {
            return FeedPoint {
                line: last_line,
                col: u16::MAX - 1,
            };
        }
        FeedPoint { line, col }
    }

    /// Purpose: Apply one mouse-wheel scroll to the thread pane.
    /// Input: The scroll lines (positive is down) and the screen position.
    /// Output: None. The feed scroll changes in place.
    /// Note: The app sends wheel events here, not to `handle_mouse`. The scroll
    /// changes only when the pointer is over the thread pane and the picker
    /// is closed.
    pub fn handle_scroll(&mut self, lines: i32, col: u16, row: u16) {
        if self.picker.is_some() || !contains(self.hits.main, col, row) {
            return;
        }
        let delta = lines.unsigned_abs() as usize;
        // The feed scroll counts lines up from the newest message.
        self.feed_scroll = if lines > 0 {
            self.feed_scroll.saturating_sub(delta)
        } else {
            self.feed_scroll.saturating_add(delta)
        };
    }

    /// Move the feed so that the scrollbar thumb sits at the screen row. A
    /// row above or below the track counts as the first or last track cell.
    fn drag_feed_scrollbar(&mut self, row: u16) {
        let bar = self.hits.feed_scrollbar;
        if bar.height == 0 {
            return;
        }
        let cell = row.clamp(bar.y, bar.bottom() - 1) - bar.y;
        self.feed_scroll = feed_scroll_for_cell(cell, bar.height, self.hits.feed_line_count);
    }

    fn click(&mut self, col: u16, row: u16) -> HubKeyOutcome {
        let hint_key = self
            .hits
            .footer_hints
            .iter()
            .find(|(rect, _)| contains(*rect, col, row))
            .and_then(|(_, key)| *key);
        if let Some(key) = hint_key {
            // A click on a footer hint is the same as a press of its key.
            return self.handle_key(&key.to_key_event());
        }
        self.click_area(col, row);
        HubKeyOutcome::Changed
    }

    fn click_area(&mut self, col: u16, row: u16) {
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
            if self.is_composing {
                self.composer
                    .handle_mouse(MouseEventKind::Down(MouseButton::Left), col, row);
            }
            return;
        }
        if contains(self.hits.dm_toggle, col, row) {
            if let Err(error) = self.toggle_dm_filter() {
                tracing::warn!(%error, "hub browser: feed load failed");
            }
            return;
        }
        if !contains(self.hits.sidebar, col, row) {
            return;
        }
        if let Some((_, hit)) = self.hits.rows.iter().find(|(y, _)| *y == row)
            && hit.is_selectable()
            && self.rows().contains(hit)
        {
            let row = hit.clone();
            self.stop_compose();
            self.select(Some(row));
        }
    }

    fn open_picker(&mut self) {
        let members = self.picker_members();
        let current = members.iter().position(|h| *h == self.perspective);
        self.picker = Some(current.unwrap_or(0));
        self.picker_top = 0;
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

    /// Leave the post box. An empty draft releases its destination.
    fn stop_compose(&mut self) {
        self.is_composing = false;
        if self.composer.is_empty() {
            self.compose_target = None;
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
        let names = self.completion_names();
        match self.composer.handle_key(key, &names) {
            ComposerKey::Exit => self.stop_compose(),
            ComposerKey::Send => {
                let text = self.composer.text().trim().to_string();
                if let (false, Some(to)) = (text.is_empty(), self.post_target()) {
                    return HubKeyOutcome::Post { to, text };
                }
            }
            ComposerKey::Edited => {}
        }
        HubKeyOutcome::Changed
    }

    fn move_selection(&mut self, rows: &[Row], step: isize) {
        let mut idx = self.selected_index(rows).unwrap_or(0) as isize;
        loop {
            idx += step;
            if idx < 0 || idx as usize >= rows.len() {
                return;
            }
            if rows[idx as usize].is_selectable() {
                self.select(Some(rows[idx as usize].clone()));
                return;
            }
        }
    }

    fn select(&mut self, row: Option<Row>) {
        if let Err(error) = self.set_selected(row) {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
    }

    fn set_selected(&mut self, row: Option<Row>) -> Result<()> {
        self.selected = row;
        self.feed_scroll = 0;
        self.feed_selection = None;
        self.load_feed(true)
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
        let feed = messages.split_off(skip);
        // A drop or replacement can move text under the stored line indices.
        if self.feed_selection.is_some() && !feed.starts_with(&self.feed) {
            self.feed_selection = None;
        }
        self.feed = feed;
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
            if is_person_member(&self.snapshot.meta, &self.person) {
                agent_hub::mark_read(&self.hub, &self.person, &conv, count)?;
            } else {
                self.unsaved_read.insert(conv.clone(), count);
            }
            self.snapshot.read.insert(conv.clone(), count);
            self.snapshot.unread_counts.insert(conv, 0);
        }
        Ok(())
    }
}

/// Purpose: Read the hub as one member sees it.
/// Input: The hub, the perspective handle, the person handle, and read
///     positions that the cursor file does not have yet.
/// Output: A snapshot with a count for every channel and every DM row.
/// Note: The person sees every member as a DM row. Another member sees only
///     the DMs that it is in.
pub fn load_snapshot(
    hub: &store::Hub,
    perspective: &str,
    person: &str,
    read_floor: &BTreeMap<String, usize>,
) -> Result<HubSnapshot> {
    let mut meta = hub.meta()?;
    for member in &mut meta.members {
        member.active = member.is_live();
    }
    let mut read = hub.cursors(perspective)?.read;
    for (conv, count) in read_floor {
        let position = read.entry(conv.clone()).or_default();
        *position = (*position).max(*count);
    }
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
/// Input: The snapshot, the perspective handle, and the DM filter.
/// Output: The channel rows, then the DM rows of the perspective member.
/// Note: The active filter keeps a DM with unread messages, so that a message
///     from an agent that stopped is not hidden.
pub fn sidebar_rows(snapshot: &HubSnapshot, perspective: &str, dm_filter: DmFilter) -> Vec<Row> {
    sidebar_rows_with_selection(snapshot, perspective, dm_filter, None)
}

/// Purpose: List sidebar rows and retain the selected DM after its unread count clears.
/// Input: The sidebar data and the selected row, if any.
/// Output: The channel and DM rows in display order.
fn sidebar_rows_with_selection(
    snapshot: &HubSnapshot,
    perspective: &str,
    dm_filter: DmFilter,
    selected: Option<&Row>,
) -> Vec<Row> {
    let mut rows = vec![Row::Section("Channels")];
    for channel in &snapshot.meta.channels {
        rows.push(Row::Channel(channel.name.clone()));
    }
    rows.push(Row::Section(DM_SECTION));
    for peer in &snapshot.dm_peers {
        let conv = store::dm_conv(perspective, peer);
        let is_selected = matches!(selected, Some(Row::Dm { peer: selected_peer, conv: selected_conv })
            if selected_peer == peer && selected_conv == &conv);
        let is_listed = match dm_filter {
            DmFilter::All => true,
            DmFilter::Active => {
                is_selected || is_active_peer(snapshot, peer) || snapshot.unread(&conv) > 0
            }
        };
        if is_listed {
            rows.push(Row::Dm {
                peer: peer.clone(),
                conv,
            });
        }
    }
    rows
}

/// True when `peer` is a person or a live agent.
fn is_active_peer(snapshot: &HubSnapshot, peer: &str) -> bool {
    snapshot
        .meta
        .member(peer)
        .is_some_and(|m| m.kind == MemberKind::Person || m.active)
}

/// Where a post from the person goes for `row`.
pub fn post_target(row: &Row) -> Option<String> {
    match row {
        Row::Section(_) => None,
        Row::Channel(name) => Some(format!("#{name}")),
        Row::Dm { peer, .. } => Some(format!("@{peer}")),
    }
}

/// True when `handle` belongs to a person member, so its cursors are safe to save.
fn is_person_member(meta: &store::HubMeta, handle: &str) -> bool {
    meta.member(handle)
        .is_some_and(|member| member.kind == MemberKind::Person)
}

fn clamp_selection(rows: &[Row], selected: usize) -> usize {
    let idx = selected.min(rows.len().saturating_sub(1));
    if rows.get(idx).is_some_and(Row::is_selectable) {
        return idx;
    }
    rows.iter().position(Row::is_selectable).unwrap_or(0)
}

/// Purpose: Find the feed scroll for a press on one scrollbar cell.
/// Input: The cell index in the track, the track height (also the feed
///     height), and the count of feed lines.
/// Output: The feed scroll, in lines up from the newest line.
/// Note: The scrollbar counts its offset down from the top. The feed counts
///     its scroll up from the bottom.
fn feed_scroll_for_cell(cell: u16, track: u16, line_count: usize) -> usize {
    let max_scroll = line_count.saturating_sub(track as usize);
    let total = u16::try_from(line_count).unwrap_or(u16::MAX);
    let offset = match scrollbar_click_to_offset(cell, track, total, track) {
        ScrollbarClickResult::Top => 0,
        ScrollbarClickResult::Bottom => max_scroll,
        ScrollbarClickResult::Offset(offset) => offset.min(max_scroll),
    };
    max_scroll - offset
}

fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom()
}

/// The style of each part of a feed line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// An agent name. The number picks the name color.
    Agent(usize),
    Person,
    Time,
    Text,
    Mention,
    /// A URL or a file path in message text. A wrapped link has one span
    /// per line, and each span keeps the target of the full link.
    Link(LinkTarget),
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
        // Find the links before the wrap, because a wrap can split a long link.
        let links = scan_links(paragraph);
        let mut cursor = 0;
        for chunk in textwrap::wrap(paragraph, body_width) {
            let mut line = vec![(indent.clone(), Part::Text)];
            match paragraph[cursor..].find(chunk.as_ref()) {
                Some(offset) => {
                    let start = cursor + offset;
                    cursor = start + chunk.len();
                    line.extend(body_spans(paragraph, start..cursor, &links, names.mentions));
                }
                // The wrap changed the text, so the line has its own links.
                None => line.extend(body_spans(
                    &chunk,
                    0..chunk.len(),
                    &scan_links(&chunk),
                    names.mentions,
                )),
            }
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

/// Purpose: Split one wrapped body line into styled spans.
/// Input: The paragraph, the byte range of the line in it, the links of the
///     paragraph, and the known `@handle`, `@here`, and `#channel` words.
/// Output: Link spans for URLs and file paths, mention spans, and plain text.
/// Note: A link span keeps the target of the full link, also when the line
///     shows only a part of the link.
fn body_spans(
    paragraph: &str,
    line: Range<usize>,
    links: &[(Range<usize>, LinkTarget)],
    mentions: &BTreeSet<String>,
) -> Vec<(String, Part)> {
    let mut spans = Vec::new();
    let mut rest_start = line.start;
    for (range, target) in links {
        let start = range.start.max(rest_start);
        let end = range.end.min(line.end);
        if start >= end {
            continue;
        }
        spans.extend(mention_spans(&paragraph[rest_start..start], mentions));
        spans.push((
            paragraph[start..end].to_string(),
            Part::Link(target.clone()),
        ));
        rest_start = end;
    }
    spans.extend(mention_spans(&paragraph[rest_start..line.end], mentions));
    spans
}

/// Purpose: Find the URLs and file paths in one line of text.
/// Input: The line text.
/// Output: The byte range and target of each link, in order, with no overlap.
/// Note: This uses the link scanner of the scrollback, so the hub finds the
///     same links.
fn scan_links(text: &str) -> Vec<(Range<usize>, LinkTarget)> {
    let mut overlay = LinkOverlay::new();
    let line = ratatui::text::Line::from(text.to_string());
    scan_lines_for_url_overlays(std::iter::once((0, &line, None)), 0, &[], &mut overlay);
    if overlay.links().is_empty() {
        return Vec::new();
    }
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(idx, _)| idx)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut links: Vec<(Range<usize>, LinkTarget)> = overlay
        .links()
        .iter()
        .filter_map(|link| {
            let start = byte_at_col(text, &boundaries, link.col_start)?;
            let end = byte_at_col(text, &boundaries, link.col_end)?;
            (start < end).then(|| (start..end, link.target.clone()))
        })
        .collect();
    links.sort_by_key(|(range, _)| range.start);
    links.dedup_by(|next, prev| next.0.start < prev.0.end);
    links
}

/// Purpose: Find the byte index where display column `col` starts.
/// Input: The text, its char boundaries (with the text end), and the column.
/// Output: The first boundary whose prefix is `col` columns wide or wider.
/// Note: The width is the string width of the prefix, as the link scanner
///     counts it. A sum of char widths differs for emoji ZWJ sequences. The
///     prefix width only grows, so a binary search needs few measurements.
fn byte_at_col(text: &str, boundaries: &[usize], col: u16) -> Option<usize> {
    let col = usize::from(col);
    let idx = boundaries.partition_point(|end| text[..*end].width() < col);
    boundaries.get(idx).copied()
}

/// Purpose: Find where a feed span points.
/// Input: The span text and part, and the hub data.
/// Output: The member, channel, or link target. Plain spans have none.
fn span_target(text: &str, part: &Part, meta: &store::HubMeta) -> Option<SpanTarget> {
    match part {
        Part::Agent(_) | Part::Person | Part::Mention => {
            if let Some(name) = text.strip_prefix('#') {
                return Some(SpanTarget::Channel(name.to_string()));
            }
            let handle = text.strip_prefix('@')?;
            meta.member(handle)?;
            Some(SpanTarget::Member(handle.to_string()))
        }
        Part::Link(link) => Some(SpanTarget::Link(link.clone())),
        _ => None,
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
    let previous_feed_width = state.hits.feed.width;
    state.hits = HitAreas::default();
    state.cursor = None;
    if area.width < 30 || area.height < 10 {
        state.feed_selection = None;
        // No popup is drawn, so no key may pick a candidate.
        state.composer.close_popup();
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
    state.hits.main = main;
    render_main(buf, main, previous_feed_width, state, &theme);
    render_footer(buf, footer, state);
    if state.is_composing {
        let compose = state.hits.compose;
        state.composer.render_popup(buf, compose, main, &theme);
    }
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
    let title = if meta.is_session_hub() {
        "this session (private)"
    } else {
        meta.name.as_str()
    };
    x = span_at(
        buf,
        x,
        bar.y,
        title,
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
    let mut chip_style = if state.is_person_view() {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_highlight)
    } else {
        Style::default().fg(theme.bg_base).bg(theme.accent_plan)
    };
    if state.hover == Some(HoverKey::Chip) {
        chip_style = chip_style.add_modifier(Modifier::UNDERLINED);
    }
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
    let selected_index = state.selected_index(&rows);
    let selected_line = screen_rows
        .iter()
        .position(|line| line.is_some() && *line == selected_index)
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
        let toggle = render_sidebar_row(
            buf,
            row_area,
            &rows[idx],
            Some(idx) == selected_index,
            state,
            theme,
        );
        if toggle.width > 0 {
            state.hits.dm_toggle = toggle;
        }
        state.hits.rows.push((y, rows[idx].clone()));
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

/// Draw one sidebar row. Return the DM filter toggle area, or an empty Rect.
fn render_sidebar_row(
    buf: &mut Buffer,
    area: Rect,
    row: &Row,
    is_selected: bool,
    state: &HubBrowserState,
    theme: &Theme,
) -> Rect {
    let is_hovered =
        row.is_selectable() && state.pointer.is_some_and(|(x, y)| contains(area, x, y));
    let bg = if is_selected {
        theme.bg_highlight
    } else if is_hovered {
        theme.bg_hover
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
            let mut toggle = Rect::default();
            let mut title_end = area.right();
            if *title == DM_SECTION {
                let is_hovered = state.hover == Some(HoverKey::DmToggle);
                toggle = render_dm_toggle(buf, area, state.dm_filter, is_hovered, theme);
                title_end = toggle.x.saturating_sub(1);
            }
            span_at(buf, area.x + 2, area.y, title, style, title_end);
            return toggle;
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
    Rect::default()
}

/// Draw the all/active chip at the right end of the DM section row.
fn render_dm_toggle(
    buf: &mut Buffer,
    area: Rect,
    dm_filter: DmFilter,
    is_hovered: bool,
    theme: &Theme,
) -> Rect {
    let label = dm_filter.label();
    let width = label.width() as u16;
    let x = area.right().saturating_sub(width + 1).max(area.x);
    let mut style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_highlight);
    if is_hovered {
        style = style.bg(theme.bg_hover).add_modifier(Modifier::UNDERLINED);
    }
    span_at(buf, x, area.y, label, style, area.right());
    Rect {
        x,
        y: area.y,
        width,
        height: 1,
    }
}

fn render_main(
    buf: &mut Buffer,
    area: Rect,
    previous_feed_width: u16,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let inner = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(4),
        ..area
    };
    let Some(row) = state.selected_row() else {
        return;
    };
    render_conversation_header(buf, inner, &row, state, theme);
    // The post box never covers the header: the 3 header rows and the
    // notice row. The text scrolls inside a short box.
    let max_compose_rows = inner.height.saturating_sub(4).max(3);
    let compose_rows = compose_rows(state, inner.width).min(max_compose_rows);
    let compose_y = inner.bottom().saturating_sub(compose_rows + 1);
    let feed = Rect {
        y: inner.y + 3,
        height: compose_y.saturating_sub(inner.y + 4),
        ..inner
    };
    if feed.width != previous_feed_width || feed.height == 0 {
        state.feed_selection = None;
    }
    // The scrollbar sits in the right margin, so the feed text does not wrap
    // again when the feed starts to overflow.
    let scrollbar = Rect {
        x: area.right().saturating_sub(1),
        width: 1,
        ..feed
    };
    render_feed(buf, feed, scrollbar, &row, state, theme);
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
        height: compose_rows,
        ..inner
    };
    render_compose(buf, compose, &row, state, theme);
}

/// Rows of the post box at `width`: the borders and the draft text rows.
fn compose_rows(state: &HubBrowserState, width: u16) -> u16 {
    let text_rows = if state.composer.is_empty() || !state.is_person_view() {
        1
    } else {
        state.composer.text_rows(compose_text_width(width))
    };
    text_rows + 2
}

/// Columns of draft text inside a post box of `width`: borders and padding off.
fn compose_text_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
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
    scrollbar: Rect,
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
    state.hits.feed_text = lines
        .iter()
        .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
        .collect();
    let pointer = state.pointer;
    for (i, line) in lines[start..end].iter().enumerate() {
        let y = top + i as u16;
        let mut x = area.x;
        for (text, part) in line {
            let target = span_target(text, part, &state.snapshot.meta);
            let mut style = part_style(part, theme);
            let end_x = x.saturating_add(text.width() as u16).min(area.right());
            if target.is_some()
                && pointer.is_some_and(|(px, py)| py == y && (x..end_x).contains(&px))
            {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            let start_x = x;
            x = span_at(buf, x, y, text, style, area.right());
            if let Some(target) = target
                && x > start_x
            {
                state.hits.feed_spans.push(FeedSpanHit {
                    row: y,
                    cols: start_x..x,
                    target,
                });
            }
        }
        let index = start + i;
        if let Some(columns) = state.feed_selection.and_then(|s| s.columns_on(index)) {
            highlight_feed_line(
                buf,
                (area.x, y),
                x,
                &state.hits.feed_text[index],
                columns,
                theme,
            );
        }
    }
    state.hits.feed = area;
    state.hits.feed_top = top;
    state.hits.feed_first_line = start;
    if max_scroll > 0 {
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let offset = u16::try_from(max_scroll - state.feed_scroll).unwrap_or(u16::MAX);
        let is_following = state.feed_scroll == 0;
        render_scrollbar(
            buf,
            Some(scrollbar),
            total,
            area.height,
            offset,
            is_following,
        );
        state.hits.feed_scrollbar = scrollbar;
        state.hits.feed_line_count = lines.len();
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

/// Purpose: Paint a feed selection across every cell of each selected grapheme.
/// Input: The drawn line origin, end column, text, and selected columns.
/// Output: The selection highlight, clipped to the drawn text.
fn highlight_feed_line(
    buf: &mut Buffer,
    (x, y): (u16, u16),
    text_end: u16,
    text: &str,
    columns: Range<u16>,
    theme: &Theme,
) {
    let start = grapheme_cells_at(text, columns.start).map_or(columns.start, |cells| cells.start);
    let end = col_past_grapheme(text, columns.end.saturating_sub(1));
    let from_x = x.saturating_add(start);
    let to_x = x.saturating_add(end).min(text_end);
    for x in from_x..to_x {
        if let Some(cell) = buf.cell_mut((x, y)) {
            crate::scrollback::text_selection::apply_selection_highlight(theme, cell);
        }
    }
}

/// Purpose: Read the text that a feed selection covers.
/// Input: The plain text of every feed line, and the selection.
/// Output: The selected text. Lines join with `\n`, without trailing spaces.
fn selected_feed_text(lines: &[String], selection: &FeedSelection) -> String {
    let (start, end) = selection.ordered();
    let mut out = Vec::new();
    for (idx, line) in lines.iter().enumerate().take(end.line + 1).skip(start.line) {
        let Some(columns) = selection.columns_on(idx) else {
            continue;
        };
        let text = slice_display_cols(line, columns.start, columns.end);
        out.push(text.trim_end().to_string());
    }
    out.join("\n")
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

fn part_style(part: &Part, theme: &Theme) -> Style {
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
        Part::Link(_) => Style::default()
            .fg(theme.accent_user)
            .add_modifier(Modifier::UNDERLINED),
        Part::Muted => Style::default().fg(theme.gray),
        Part::Rule => Style::default().fg(theme.gray_dim),
        Part::NewRule => Style::default().fg(theme.accent_error),
    }
}

/// Purpose: Draw the post box with its destination in the top border.
/// Input: The buffer, the post box area, the selected row, and the state.
/// Output: None. The state keeps the post box area and the cursor cell.
fn render_compose(
    buf: &mut Buffer,
    area: Rect,
    row: &Row,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let is_hovered = state.hover == Some(HoverKey::Compose);
    let border = if state.is_composing || is_hovered {
        theme.prompt_border_active
    } else if state.is_person_view() {
        theme.prompt_border
    } else {
        theme.gray_dim
    };
    let border_style = Style::default().fg(border);
    let inner_w = area.width.saturating_sub(2) as usize;
    let to = state
        .post_target()
        .or_else(|| post_target(row))
        .unwrap_or_default();
    let title = if state.is_person_view() && !state.composer.is_empty() {
        format!(" to {to} ")
    } else {
        String::new()
    };
    let top_fill = inner_w.saturating_sub(title.width() + 1);
    span_at(
        buf,
        area.x,
        area.y,
        &format!("╭─{title}{}╮", "─".repeat(top_fill)),
        border_style,
        area.right(),
    );
    for y in area.y + 1..area.bottom().saturating_sub(1) {
        span_at(buf, area.x, y, "│", border_style, area.right());
        span_at(buf, area.right() - 1, y, "│", border_style, area.right());
    }
    span_at(
        buf,
        area.x,
        area.bottom().saturating_sub(1),
        &format!("╰{}╯", "─".repeat(inner_w)),
        border_style,
        area.right(),
    );
    state.hits.compose = area;
    let text_area = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: compose_text_width(area.width),
        height: area.height.saturating_sub(2),
    };
    if !state.is_person_view() {
        let text = format!(
            "Viewing as @{} · read only · press p to switch back to you",
            state.perspective
        );
        let style = theme.muted().add_modifier(Modifier::ITALIC);
        span_at(
            buf,
            text_area.x,
            text_area.y,
            &text,
            style,
            text_area.right(),
        );
        return;
    }
    if state.composer.is_empty() {
        span_at(
            buf,
            text_area.x,
            text_area.y,
            &format!("Message {to}"),
            theme.muted(),
            text_area.right(),
        );
        if state.is_composing {
            state.cursor = Some((text_area.x, text_area.y));
        }
        return;
    }
    state.cursor = state
        .composer
        .render_text(buf, text_area, state.is_composing);
}

fn render_footer(buf: &mut Buffer, bar: Rect, state: &mut HubBrowserState) {
    let hints = footer_hints(state);
    let hovered = match state.hover {
        Some(HoverKey::Hint(idx)) => Some(idx),
        _ => None,
    };
    let footer = Rect {
        x: bar.x + 2,
        width: bar.width.saturating_sub(2),
        ..bar
    };
    let rects = ShortcutsBar::new(&hints)
        .with_hovered(hovered)
        .render_with_hits(footer, buf);
    state.hits.footer_hints = hints
        .iter()
        .zip(rects)
        .map(|(hint, rect)| (rect, hint.click_key()))
        .collect();
}

/// The footer hints for the current mode. A click on a hint presses its key.
fn footer_hints(state: &HubBrowserState) -> Vec<HintItem> {
    use crate::key;
    if state.picker.is_some() {
        return vec![
            HintItem::paired(key!(Up), key!(Down), "choose"),
            HintItem::new(key!(Enter), "view as"),
            HintItem::new(key!(Esc), "cancel"),
        ];
    }
    if state.is_composing {
        return vec![
            HintItem::new(key!(Enter), "send"),
            HintItem::new(key!(Esc), "stop writing"),
        ];
    }
    // Clickable hints come first, so a narrow footer keeps them.
    let mut hints = Vec::new();
    if state.is_person_view() {
        hints.push(HintItem::new(key!(Tab), "write"));
    }
    hints.extend([
        HintItem::new(key!('p'), "perspective"),
        HintItem::new(key!('a'), "all/active DMs"),
        HintItem::new(key!(Esc), "close hub"),
        HintItem::paired(key!(Up), key!(Down), "conversations"),
        HintItem::paired(key!(PageUp), key!(PageDown), "scroll"),
    ]);
    hints
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
    let first = picker_viewport_top(state.picker_top, highlighted, visible, members.len());
    state.picker_top = first;
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

/// Purpose: Find the first visible picker row.
/// Input: The last first row, the highlighted row, the visible row count, and
///     the member count.
/// Output: The last first row when the highlighted row is visible in it.
///     Otherwise, the nearest first row that shows the highlighted row.
fn picker_viewport_top(top: usize, highlighted: usize, visible: usize, count: usize) -> usize {
    let top = if highlighted < top {
        highlighted
    } else if highlighted >= top + visible {
        (highlighted + 1).saturating_sub(visible)
    } else {
        top
    };
    top.min(count.saturating_sub(visible))
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
    use crossterm::event::{KeyEvent, KeyModifiers};

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

    fn member(handle: &str, kind: MemberKind) -> store::Member {
        store::Member {
            handle: handle.to_string(),
            label: "main".to_string(),
            kind,
            session_id: None,
            active: true,
            disconnected: false,
            joined_ms: 0,
            pid: None,
        }
    }

    fn hub_with_dms() -> (tempfile::TempDir, store::Hub) {
        let root = tempfile::tempdir().unwrap();
        let hub = store::Hub::create(root.path(), "demo").unwrap();
        hub.update_meta(|meta| {
            for handle in ["front", "back", "me"] {
                let kind = if handle == "me" {
                    MemberKind::Person
                } else {
                    MemberKind::Agent
                };
                meta.members.push(member(handle, kind));
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
        HubBrowserState::open(hub, "me", None).unwrap()
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
        let mut state = HubBrowserState::open(hub, "me", Some("#general")).unwrap();
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
        assert_eq!(
            state.composer.text(),
            "hi all",
            "the post has not succeeded yet"
        );
        state.finish_post("#general", Ok("message-id".to_string()));
        assert!(state.composer.is_empty());
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
            state.select(Some(selected.clone()));
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
            assert_eq!(state.selected_index(&state.rows()), Some(idx + 1));
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
    fn a_row_click_after_a_post_releases_the_old_destination() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("hi");
        assert!(matches!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post { .. }
        ));
        state.finish_post("#general", Ok("message-id".to_string()));
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let (y, _) = state
            .hits
            .rows
            .iter()
            .find(|(_, row)| *row == dm("front", "me"))
            .unwrap();
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), 3, *y);
        assert!(!state.is_composing);
        assert_eq!(state.post_target().as_deref(), Some("@front"));
    }

    #[test]
    fn switching_back_to_the_person_marks_only_the_shown_conversation_read() {
        let (_root, hub) = hub_with_dms();
        let back_dm = store::dm_conv("back", "me");
        hub.append(&message("back", back_dm.clone(), "unread", store::now_ms()))
            .unwrap();
        let mut state = open(hub.clone());
        state.set_perspective("front").unwrap();
        let front_dm = store::dm_conv("front", "me");
        let row = state
            .rows()
            .into_iter()
            .find(|row| row.conv().as_deref() == Some(front_dm.as_str()))
            .unwrap();
        state.select(Some(row));

        state.set_perspective("me").unwrap();

        assert_eq!(
            state.selected_row().and_then(|row| row.conv()),
            Some(front_dm)
        );
        assert_eq!(hub.cursors("me").unwrap().read.get(&back_dm), None);
        assert_eq!(state.snapshot.unread(&back_dm), 1);
    }

    #[test]
    fn an_agent_that_takes_the_person_handle_keeps_its_cursors() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub.clone(), "tony", None).unwrap();
        assert_eq!(state.person, "tony");
        hub.update_meta(|meta| meta.members.push(member("tony", MemberKind::Agent)))
            .unwrap();

        state.reload().unwrap();

        assert_eq!(state.person, "tony-2");
        assert!(state.is_person_view());
        assert!(hub.cursors("tony").unwrap().read.is_empty());
        assert!(
            hub.cursors("tony-2").unwrap().read.is_empty(),
            "not reserved yet"
        );
        assert_eq!(state.snapshot.unread("#general"), 0, "kept in memory");

        // The first post reserves the person handle this way.
        agent_hub::ensure_person(&hub, state.person_name()).unwrap();
        state.reload().unwrap();

        assert_eq!(state.person, "tony-2");
        assert_eq!(
            hub.cursors("tony-2").unwrap().read.get("#general"),
            Some(&1)
        );
    }

    #[test]
    fn mouse_clicks_select_rows_and_switch_perspective() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let (y, _) = state
            .hits
            .rows
            .iter()
            .find(|(_, row)| *row == dm("front", "me"))
            .unwrap();
        let down = MouseEventKind::Down(MouseButton::Left);
        state.handle_mouse(down, 3, *y);
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

    #[test]
    fn active_dm_filter_hides_gone_agents_but_keeps_unread_dms() {
        let (_root, hub) = hub_with_dms();
        hub.update_meta(|meta| {
            meta.members
                .iter_mut()
                .find(|m| m.handle == "back")
                .unwrap()
                .active = false;
            let mut old = member("old", MemberKind::Agent);
            old.active = false;
            meta.members.push(old);
        })
        .unwrap();
        let conv = store::dm_conv("me", "old");
        hub.append(&message("old", conv, "final report", store::now_ms()))
            .unwrap();
        let mut state = open(hub);
        let all = vec![dm("front", "me"), dm("back", "me"), dm("old", "me")];
        assert!(state.rows().ends_with(&all));

        state.handle_key(&key(KeyCode::Char('a')));
        assert_eq!(state.dm_filter, DmFilter::Active);
        assert!(
            state
                .rows()
                .ends_with(&[dm("front", "me"), dm("old", "me")])
        );
        assert!(!state.rows().contains(&dm("back", "me")));

        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let toggle = state.hits.dm_toggle;
        assert!(buffer_text(&buf).contains("Direct messages"));
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), toggle.x, toggle.y);
        assert_eq!(state.dm_filter, DmFilter::All);
        assert!(state.rows().ends_with(&all));
    }

    #[test]
    fn reading_an_inactive_dm_keeps_the_selected_row_and_post_target() {
        let (_root, hub) = hub_with_dms();
        hub.update_meta(|meta| {
            meta.members
                .iter_mut()
                .find(|m| m.handle == "front")
                .unwrap()
                .active = false;
        })
        .unwrap();
        let conv = store::dm_conv("me", "front");
        hub.append(&message(
            "front",
            conv.clone(),
            "final report",
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('a')));
        state.handle_key(&key(KeyCode::Down));

        assert_eq!(state.snapshot.unread(&conv), 0);
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
        assert_eq!(state.post_target().as_deref(), Some("@front"));
        assert_eq!(state.feed.last().unwrap().text, "final report");

        state.reload().unwrap();
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
        assert_eq!(state.post_target().as_deref(), Some("@front"));

        state.handle_key(&key(KeyCode::Down));
        assert_eq!(state.selected_row(), Some(dm("back", "me")));
        assert!(!state.rows().contains(&dm("front", "me")));
    }

    #[test]
    fn a_drag_in_the_thread_copies_the_selected_text() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let hits = &state.hits;
        let line = hits
            .feed_text
            .iter()
            .position(|t| t.contains("hello"))
            .unwrap();
        let col = hits.feed_text[line].find("hello").unwrap() as u16;
        let x = hits.feed.x + col;
        let y = hits.feed_top + (line - hits.feed_first_line) as u16;

        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), x, y);
        assert_eq!(state.copy_text, None, "a click does not copy");
        assert_eq!(state.feed_selection, None);

        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        state.handle_mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y);
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), x + 4, y);
        assert_eq!(state.copy_text.as_deref(), Some("hello"));
        assert!(
            state.feed_selection.is_some(),
            "the selection stays visible"
        );

        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), 3, 3);
        assert_eq!(state.feed_selection, None, "a click elsewhere clears it");
    }

    #[test]
    fn selected_feed_text_joins_lines_by_columns() {
        let lines = vec!["ab cd".to_string(), "  ef  ".to_string(), "gh".to_string()];
        let selection = FeedSelection {
            anchor: FeedPoint { line: 2, col: 0 },
            head: FeedPoint { line: 0, col: 3 },
            is_dragging: false,
        };
        assert_eq!(selected_feed_text(&lines, &selection), "cd\n  ef\ng");
    }

    #[test]
    fn copied_columns_match_the_highlight_for_extended_graphemes() {
        let text = "👩‍💻e\u{301}x";
        let lines = [text.to_string()];
        let mut selection = FeedSelection {
            anchor: FeedPoint { line: 0, col: 2 },
            head: FeedPoint { line: 0, col: 3 },
            is_dragging: false,
        };
        assert_eq!(selected_feed_text(&lines, &selection), "e\u{301}x");

        selection.anchor.col = 1;
        selection.head.col = 2;
        assert_eq!(selected_feed_text(&lines, &selection), "👩‍💻e\u{301}");

        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        highlight_feed_line(
            &mut buf,
            (0, 0),
            4,
            text,
            selection.columns_on(0).unwrap(),
            &Theme::current(),
        );
        assert_eq!(buf[(0, 0)], buf[(1, 0)]);
        assert_eq!(buf[(1, 0)], buf[(2, 0)]);
        assert_ne!(buf[(2, 0)], buf[(3, 0)]);
    }

    #[test]
    fn a_drag_cancels_when_the_feed_drops_its_first_message() {
        let (_root, hub) = hub_with_dms();
        let start_ms = store::now_ms();
        for index in 1..FEED_LIMIT {
            let text = format!("message {index}");
            hub.append(&message("me", "#general".to_string(), &text, start_ms))
                .unwrap();
        }
        let mut state = open(hub.clone());
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let x = state.hits.feed.x + BODY_INDENT as u16;
        let y = state.hits.feed.bottom() - 1;
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        state.handle_mouse(MouseEventKind::Drag(MouseButton::Left), x + 3, y);
        assert!(state.feed_selection.is_some());

        hub.append(&message(
            "me",
            "#general".to_string(),
            "new arrival",
            start_ms,
        ))
        .unwrap();
        state.reload().unwrap();
        render_hub_browser(&mut buf, area, &mut state);
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        assert_eq!(state.feed_selection, None);
        assert_eq!(state.copy_text, None);
    }

    #[test]
    fn a_drag_cancels_when_the_feed_wrap_width_changes() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let x = state.hits.feed.x + BODY_INDENT as u16;
        let y = state.hits.feed.bottom() - 1;
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        state.handle_mouse(MouseEventKind::Drag(MouseButton::Left), x + 3, y);

        state.reload().unwrap();
        render_hub_browser(&mut buf, area, &mut state);
        assert!(state.feed_selection.is_some());

        let narrower = Rect::new(0, 0, 100, 30);
        render_hub_browser(&mut buf, narrower, &mut state);
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        assert_eq!(state.feed_selection, None);
        assert_eq!(state.copy_text, None);
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
    fn feed_scroll_for_cell_maps_track_ends_to_oldest_and_newest() {
        // 100 lines in a 10-row feed: the scroll runs from 0 (newest) to 90.
        assert_eq!(feed_scroll_for_cell(0, 10, 100), 90);
        assert_eq!(feed_scroll_for_cell(9, 10, 100), 0);
        let middle = feed_scroll_for_cell(5, 10, 100);
        assert!(middle > 0 && middle < 90, "{middle}");
        // A feed that fits has no scroll.
        assert_eq!(feed_scroll_for_cell(0, 10, 4), 0);
    }

    #[test]
    fn feed_scrollbar_drag_scrolls_a_long_thread() {
        let (_root, hub) = hub_with_dms();
        let start_ms = store::now_ms();
        for i in 0..80 {
            let text = format!("line {i}");
            hub.append(&message("me", "#general".to_string(), &text, start_ms + i))
                .unwrap();
        }
        let mut state = open(hub);
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
        let area = Rect::new(0, 0, 100, 24);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let bar = state.hits.feed_scrollbar;
        assert_eq!(bar.width, 1, "a long thread shows a scrollbar");
        assert_eq!(
            bar.x,
            area.right() - 1,
            "the scrollbar is at the right edge"
        );

        // Press the bottom cell, then drag above the track: the feed goes to the top.
        state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            bar.x,
            bar.bottom() - 1,
        );
        assert_eq!(state.feed_scroll, 0);
        state.handle_mouse(MouseEventKind::Drag(MouseButton::Left), bar.x - 5, 0);
        let max_scroll = state.hits.feed_line_count - bar.height as usize;
        assert_eq!(state.feed_scroll, max_scroll);
        render_hub_browser(&mut buf, area, &mut state);
        assert!(
            buffer_text(&buf).contains("line 0"),
            "{}",
            buffer_text(&buf)
        );

        // After release, a drag does not move the feed.
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), bar.x, bar.y);
        state.handle_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            bar.x,
            bar.bottom() - 1,
        );
        assert_eq!(state.feed_scroll, max_scroll);
    }

    #[test]
    fn wheel_over_the_thread_scrolls_the_feed_and_its_scrollbar() {
        let (_root, hub) = hub_with_dms();
        let start_ms = store::now_ms();
        for i in 0..80 {
            let text = format!("line {i}");
            hub.append(&message("me", "#general".to_string(), &text, start_ms + i))
                .unwrap();
        }
        let mut state = open(hub);
        let area = Rect::new(0, 0, 100, 24);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let bar = state.hits.feed_scrollbar;
        let bar_before = column_text(&buf, bar);
        let main = state.hits.main;
        let (col, row) = (main.x + main.width / 2, main.y + main.height / 2);

        state.handle_scroll(-5, col, row);
        assert_eq!(state.feed_scroll, 5, "wheel up moves to older lines");
        render_hub_browser(&mut buf, area, &mut state);
        assert_ne!(column_text(&buf, bar), bar_before, "the thumb moves");
        state.handle_scroll(3, col, row);
        assert_eq!(state.feed_scroll, 2, "wheel down moves to newer lines");

        // The wheel over the sidebar does not move the thread.
        let side = state.hits.sidebar;
        state.handle_scroll(-5, side.x + 1, side.y + 1);
        assert_eq!(state.feed_scroll, 2);
    }

    fn column_text(buf: &Buffer, column: Rect) -> String {
        (column.y..column.bottom())
            .map(|y| buf[(column.x, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn a_short_thread_has_no_scrollbar() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 100, 24);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        assert_eq!(state.hits.feed_scrollbar, Rect::default());
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
            "close hub",
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

    /// Open the demo hub with one agent message in #general and draw a frame.
    fn open_with_general_post(text: &str) -> (tempfile::TempDir, HubBrowserState) {
        let (root, hub) = hub_with_dms();
        hub.append(&message(
            "front",
            "#general".to_string(),
            text,
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        let area = buf.area;
        render_hub_browser(&mut buf, area, &mut state);
        (root, state)
    }

    fn span_cell(state: &HubBrowserState, target: &SpanTarget) -> (u16, u16) {
        let span = state
            .hits
            .feed_spans
            .iter()
            .find(|span| &span.target == target)
            .expect("span is drawn");
        (span.cols.start, span.row)
    }

    fn click(state: &mut HubBrowserState, (x, y): (u16, u16)) -> HubKeyOutcome {
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), x, y)
    }

    #[test]
    fn click_on_a_sender_handle_opens_the_dm_with_that_member() {
        let (_root, mut state) = open_with_general_post("ready");
        let cell = span_cell(&state, &SpanTarget::Member("front".to_string()));
        assert_eq!(click(&mut state, cell), HubKeyOutcome::Changed);
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
    }

    #[test]
    fn click_on_a_handle_in_another_view_returns_to_the_person() {
        let (_root, mut state) = open_with_general_post("ready");
        state.set_perspective("back").unwrap();
        state.select(Some(Row::Channel("general".to_string())));
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        let area = buf.area;
        render_hub_browser(&mut buf, area, &mut state);
        let cell = span_cell(&state, &SpanTarget::Member("front".to_string()));
        click(&mut state, cell);
        assert!(state.is_person_view());
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
    }

    #[test]
    fn click_on_a_channel_mention_opens_the_channel() {
        let (root, hub) = hub_with_dms();
        hub.update_meta(|meta| {
            meta.channels.push(store::Channel {
                name: "ops".to_string(),
                purpose: String::new(),
                created_ms: 0,
            });
        })
        .unwrap();
        hub.append(&message(
            "front",
            "#general".to_string(),
            "see #ops now",
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        let area = buf.area;
        render_hub_browser(&mut buf, area, &mut state);
        let cell = span_cell(&state, &SpanTarget::Channel("ops".to_string()));
        click(&mut state, cell);
        assert_eq!(state.selected_row(), Some(Row::Channel("ops".to_string())));
        drop(root);
    }

    #[test]
    fn click_on_a_url_asks_the_caller_to_open_it() {
        let (_root, mut state) = open_with_general_post("docs at https://example.com/a today");
        let span = state
            .hits
            .feed_spans
            .iter()
            .find(|span| matches!(span.target, SpanTarget::Link(_)))
            .expect("link span")
            .clone();
        let outcome = click(&mut state, (span.cols.start, span.row));
        assert_eq!(
            outcome,
            HubKeyOutcome::OpenLink(LinkTarget::Url("https://example.com/a".into()))
        );
    }

    #[test]
    fn a_drag_over_a_handle_selects_text_and_does_not_follow_it() {
        let (_root, mut state) = open_with_general_post("ready");
        let (x, y) = span_cell(&state, &SpanTarget::Member("front".to_string()));
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        state.handle_mouse(MouseEventKind::Drag(MouseButton::Left), x + 3, y);
        state.handle_mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
        assert!(state.copy_text.is_some());
    }

    #[test]
    fn hover_redraws_only_when_the_hovered_element_changes() {
        let (_root, mut state) = open_with_general_post("ready");
        let (x, y) = span_cell(&state, &SpanTarget::Member("front".to_string()));
        assert_eq!(
            state.handle_mouse(MouseEventKind::Moved, x, y),
            HubKeyOutcome::Changed
        );
        assert_eq!(
            state.handle_mouse(MouseEventKind::Moved, x + 1, y),
            HubKeyOutcome::Unchanged
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        let area = buf.area;
        render_hub_browser(&mut buf, area, &mut state);
        let cell = buf.cell((x, y)).unwrap();
        assert!(cell.modifier.contains(Modifier::UNDERLINED));
        let empty = (state.hits.main.x + 3, state.hits.main.y + 1);
        assert_eq!(
            state.handle_mouse(MouseEventKind::Moved, empty.0, empty.1),
            HubKeyOutcome::Changed
        );
        assert_eq!(
            state.handle_mouse(MouseEventKind::Moved, empty.0 + 1, empty.1),
            HubKeyOutcome::Unchanged
        );
    }

    #[test]
    fn click_on_a_footer_hint_presses_its_key() {
        let (_root, mut state) = open_with_general_post("ready");
        let filter_key = crate::key!('a');
        let rect = state
            .hits
            .footer_hints
            .iter()
            .find(|(_, key)| *key == Some(filter_key))
            .map(|(rect, _)| *rect)
            .expect("filter hint");
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
        assert_eq!(state.dm_filter, DmFilter::Active);
        let close = state
            .hits
            .footer_hints
            .iter()
            .find(|(_, key)| *key == Some(crate::key!(Esc)))
            .map(|(rect, _)| *rect)
            .expect("close hint");
        assert_eq!(
            state.handle_mouse(MouseEventKind::Down(MouseButton::Left), close.x, close.y),
            HubKeyOutcome::Close
        );
    }

    #[test]
    fn body_spans_mark_urls_and_paths_as_links() {
        let mentions = BTreeSet::from(["@front".to_string()]);
        let text = "ask @front about /tmp/a/b.rs or https://x.io";
        let spans = body_spans(text, 0..text.len(), &scan_links(text), &mentions);
        let links: Vec<&str> = spans
            .iter()
            .filter(|(_, part)| matches!(part, Part::Link(_)))
            .map(|(text, _)| text.as_str())
            .collect();
        assert_eq!(links, vec!["/tmp/a/b.rs", "https://x.io"]);
        assert!(spans.contains(&("@front".to_string(), Part::Mention)));
        let text: String = spans.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(text, "ask @front about /tmp/a/b.rs or https://x.io");
    }

    #[test]
    fn the_post_box_completes_handles_and_sends_multiline_text() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        for c in "hi @fr".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        assert!(state.composer.is_completing());
        state.handle_key(&key(KeyCode::Tab));
        assert!(state.is_composing, "Tab accepts the completion, not exit");
        state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        state.handle_key(&key(KeyCode::Char('x')));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "#general".to_string(),
                text: "hi @front \nx".to_string(),
            }
        );
    }

    #[test]
    fn the_post_box_grows_and_sets_the_terminal_cursor_while_composing() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        assert_eq!(state.cursor, None);
        let one_line = state.hits.compose.height;
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("a\nb\nc");
        render_hub_browser(&mut buf, area, &mut state);
        assert_eq!(state.hits.compose.height, one_line + 2);
        let (x, y) = state.cursor.expect("cursor while composing");
        assert!(contains(state.hits.compose, x, y));
        assert!(buffer_text(&buf).contains("to #general"));
    }

    #[test]
    fn a_tall_draft_in_a_short_terminal_does_not_cover_the_header() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("1\n2\n3\n4\n5\n6");
        let area = Rect::new(0, 0, 100, 10);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        assert!(
            state.hits.compose.y >= state.hits.main.y + 3,
            "post box {:?} under the header of {:?}",
            state.hits.compose,
            state.hits.main
        );
        assert!(state.hits.compose.bottom() <= area.bottom());
    }

    #[test]
    fn a_completion_popup_shows_every_candidate_that_keys_can_pick() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_key(&key(KeyCode::Char('@')));
        assert!(state.composer.is_completing());
        for height in [8, 12, 30] {
            let area = Rect::new(0, 0, 100, height);
            let mut buf = Buffer::empty(area);
            render_hub_browser(&mut buf, area, &mut state);
            if state.composer.is_completing() {
                assert_eq!(
                    state.composer.popup_row_count(),
                    state.composer.completion_items().len(),
                    "height {height}"
                );
            }
            state.handle_key(&key(KeyCode::Backspace));
            state.handle_key(&key(KeyCode::Char('@')));
        }
    }

    #[test]
    fn every_line_of_a_wrapped_link_opens_the_full_link() {
        let url = format!("https://example.com/{}", "segment/".repeat(8));
        let msg = message("front", "#general".into(), &format!("see {url} now"), 0);
        let persons = BTreeSet::new();
        let mentions = BTreeSet::new();
        let names = FeedNames {
            persons: &persons,
            mentions: &mentions,
        };
        let today = local_time(0).date_naive();
        let lines = feed_lines(std::slice::from_ref(&msg), 30, None, &names, today);
        let links: Vec<&LinkTarget> = lines
            .iter()
            .flatten()
            .filter_map(|(_, part)| match part {
                Part::Link(link) => Some(link),
                _ => None,
            })
            .collect();
        assert!(links.len() > 1, "the link wraps: {lines:?}");
        for link in links {
            assert_eq!(link, &LinkTarget::Url(url.as_str().into()));
        }
    }

    #[test]
    fn a_handle_click_in_another_view_marks_no_other_conversation_read() {
        let (_root, mut state) = open_with_general_post("ready");
        state.set_perspective("back").unwrap();
        state.select(Some(Row::Channel("general".to_string())));
        state
            .hub
            .append(&message(
                "front",
                "#general".to_string(),
                "later",
                store::now_ms(),
            ))
            .unwrap();
        state.reload().unwrap();
        let read_before = state
            .hub
            .cursors("me")
            .unwrap()
            .read
            .get("#general")
            .copied();
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        let area = buf.area;
        render_hub_browser(&mut buf, area, &mut state);
        let cell = span_cell(&state, &SpanTarget::Member("front".to_string()));
        click(&mut state, cell);
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
        let read_after = state
            .hub
            .cursors("me")
            .unwrap()
            .read
            .get("#general")
            .copied();
        assert_eq!(read_after, read_before);
        assert_eq!(state.snapshot.unread("#general"), 1);
    }

    #[test]
    fn a_hover_does_not_scroll_the_picker() {
        // Ten members, six visible rows, the last member highlighted.
        assert_eq!(picker_viewport_top(0, 9, 6, 10), 4);
        // A hover on the top visible row keeps the rows where they are.
        assert_eq!(picker_viewport_top(4, 4, 6, 10), 4);
        assert_eq!(picker_viewport_top(4, 7, 6, 10), 4);
        // Keys past an edge scroll by the smallest step.
        assert_eq!(picker_viewport_top(4, 3, 6, 10), 3);
        assert_eq!(picker_viewport_top(0, 6, 6, 10), 1);
        // Fewer members than rows always start at the top.
        assert_eq!(picker_viewport_top(3, 1, 6, 4), 0);
    }

    #[test]
    fn links_after_emoji_sequences_keep_their_text() {
        let text = "👩‍💻👩‍💻👩‍💻 see https://x.io/a now";
        let links = scan_links(text);
        assert_eq!(links.len(), 1);
        assert_eq!(&text[links[0].0.clone()], "https://x.io/a");
    }
}
