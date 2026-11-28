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
use std::sync::atomic::{AtomicU64, Ordering};

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
/// Older messages that the feed also shows above a focused old message.
const FOCUS_CONTEXT: usize = 5;
/// Widest the sidebar gets.
const SIDEBAR_MAX_WIDTH: u16 = 30;
/// Messages from one sender closer than this share one header.
const GROUP_GAP_MS: u64 = 5 * 60 * 1000;
/// Most results that one hub search shows.
const SEARCH_LIMIT: usize = 50;
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
    /// What each agent reported that it does now, by handle.
    pub presences: BTreeMap<String, store::Presence>,
    /// Estimated tokens to agents by token key: `hub.json` plus member files.
    pub tokens: BTreeMap<String, u64>,
}

/// Incremental read state of the hub files, so that a reload reads only the
/// bytes that were appended since the last read.
#[derive(Debug, Clone, Default)]
pub struct HubReadCache {
    /// The member whose counts `unread` holds.
    perspective: String,
    /// Unread state per conversation key.
    unread: BTreeMap<String, UnreadTail>,
    /// Every message of the last feed conversation.
    feed: Option<FeedTail>,
}

#[derive(Debug, Clone)]
struct UnreadTail {
    /// The read count that `unread` counts from.
    read: usize,
    tail: store::ConvTail,
    /// Messages after `read` that other members sent.
    unread: usize,
}

#[derive(Debug, Clone)]
struct FeedTail {
    conv: String,
    tail: store::ConvTail,
    messages: Vec<store::Message>,
}

/// A read position of the person that waits to be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadMark {
    pub conv: String,
    pub count: usize,
    /// The byte offset after `count` complete lines.
    pub offset: Option<store::LineOffset>,
}

/// The inputs of a background refresh. It holds no reference to the browser.
#[derive(Debug)]
pub struct HubRefreshJob {
    pub generation: u64,
    hub: store::Hub,
    person_name: String,
    person: String,
    perspective: String,
    unsaved_read: BTreeMap<String, usize>,
    /// The conversation of the selected row.
    conv: Option<String>,
    cache: HubReadCache,
}

/// The result of a background refresh.
#[derive(Debug)]
pub struct HubRefresh {
    person: String,
    /// The unsaved read positions that the refresh saved.
    saved_read: BTreeMap<String, usize>,
    snapshot: HubSnapshot,
    cache: HubReadCache,
}

impl HubRefreshJob {
    /// Purpose: Read the hub for a background refresh, off the UI thread.
    /// Input: The job.
    /// Output: The new snapshot, and the read cache with the feed conversation.
    /// Note: Like `reload_snapshot`, it saves the unsaved read positions of
    ///     the person once the person is a member. It moves no other cursor.
    pub fn run(mut self) -> Result<HubRefresh> {
        let meta = self.hub.meta()?;
        let person = agent_hub::person_handle_in(&meta, &self.person_name);
        let perspective = if self.perspective == self.person {
            person.clone()
        } else {
            self.perspective
        };
        let mut saved_read = BTreeMap::new();
        if is_person_member(&meta, &person) {
            for (conv, count) in std::mem::take(&mut self.unsaved_read) {
                agent_hub::mark_read(&self.hub, &person, &conv, count)?;
                saved_read.insert(conv, count);
            }
        }
        let read_floor = if perspective == person {
            self.unsaved_read
        } else {
            BTreeMap::new()
        };
        let snapshot = load_snapshot(
            &self.hub,
            &perspective,
            &person,
            &read_floor,
            &mut self.cache,
        )?;
        if let Some(conv) = &self.conv {
            let cached = self.cache.feed.take();
            self.cache.feed = Some(load_conv(&self.hub, conv, cached)?);
        }
        Ok(HubRefresh {
            person,
            saved_read,
            snapshot,
            cache: self.cache,
        })
    }
}

/// A new refresh generation. Every local load takes one, so a background
/// refresh that started before it is stale. The values are unique in the
/// process, so a result never applies to a browser that opened later.
fn next_refresh_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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
    /// Post a reply to the message `reply_to` in the same conversation.
    Reply {
        to: String,
        text: String,
        reply_to: String,
    },
    /// Open a URL or a file from the feed.
    OpenLink(LinkTarget),
    /// Show the session of a member.
    GoToMember(String),
    /// Stop the running turn of a member.
    StopMember(String),
    /// Pin a message of a channel, as the person.
    Pin {
        channel: String,
        message_id: String,
    },
    /// Unpin a message of a channel, as the person.
    Unpin {
        channel: String,
        message_id: String,
    },
}

/// What a clickable feed span points to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpanTarget {
    /// A member handle, without `@`.
    Member(String),
    /// A channel name, without `#`.
    Channel(String),
    Link(LinkTarget),
    /// The thread of a message, by the id of its first message.
    Thread(String),
}

/// An open search of the whole hub.
#[derive(Debug, Clone, Default)]
pub struct HubSearch {
    pub query: String,
    pub results: Vec<store::Message>,
    pub selected: usize,
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
    HeaderGo,
    HeaderStop,
    ReplyChip,
    ThreadBack,
    /// A search result, by its screen row.
    SearchRow(u16),
    /// A feed line of a message, by the feed index of the message.
    FeedMessage(usize),
    /// The `[📌 N]` pill of a channel header.
    PinPill,
    /// The pin or unpin button of the hovered feed message.
    PinChip,
    /// A row of the pinned-messages drawer, by its screen row.
    PinRow(u16),
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
    /// The reply chip of the hovered message, and that message id.
    reply_chip: Option<(Rect, String)>,
    /// Screen row and message id of each visible search result.
    search_rows: Vec<(u16, String)>,
    /// The "back" button of a thread header.
    thread_back: Rect,
    /// Feed index of the message on each visible feed line, from `feed_top` down.
    feed_line_messages: Vec<Option<usize>>,
    /// The "go to session" and "stop" buttons of a DM header.
    header_go: Rect,
    header_stop: Rect,
    /// The `[📌 N]` pill of a channel header.
    pin_pill: Rect,
    /// The pin or unpin button of the hovered message, and that message id.
    pin_chip: Option<(Rect, String)>,
    /// The open pinned-messages drawer.
    pin_drawer: Rect,
    /// Screen row and message id of each visible drawer row.
    pin_rows: Vec<(u16, String)>,
    /// The `[unpin]` button and message id of each visible drawer row.
    pin_unpins: Vec<(Rect, String)>,
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
    /// The parent of a nonempty draft, fixed when the draft starts: `Some(None)`
    /// for a top-level post. Browsing threads later does not change it.
    pinned_parent: Option<Option<String>>,
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
    /// A member that one more `x` (or stop click) stops.
    pending_stop: Option<String>,
    /// The first message of the open thread. The feed shows only the thread.
    pub thread: Option<String>,
    /// The message that the draft replies to, outside a thread view.
    pub reply_to: Option<String>,
    /// The open search, which takes the main pane.
    pub search: Option<HubSearch>,
    pub show_help: bool,
    /// A message to mark, after a search jump.
    focus_message: Option<String>,
    /// True until the next frame scrolls the focus message into view.
    scroll_to_focus: bool,
    /// True while hub-bot works on a question of the person.
    pub is_bot_thinking: bool,
    /// The pinned messages of the selected channel, in pin order.
    pub pinned: Vec<store::Message>,
    /// True while the pinned-messages drawer is open.
    pub show_pins: bool,
    /// The highlighted row of the drawer.
    pins_selected: usize,
    hits: HitAreas,
    /// What the last reads saw, so the next reload reads only new bytes.
    cache: HubReadCache,
    /// The generation of the last local load. See `next_refresh_generation`.
    refresh_generation: u64,
    /// The generation of the background refresh that runs now, if any.
    refresh_in_flight: Option<u64>,
    /// `Some` while a background refresh applies: the feed uses the cache
    /// as it is, and read positions wait here instead of a disk write.
    deferred_marks: Option<Vec<ReadMark>>,
}

impl HubBrowserState {
    /// Purpose: Open the browser on a hub, as the person.
    /// Input: The hub, the person name, and an optional `#channel` to select.
    /// Output: The state, or an error when the hub files cannot be read.
    pub fn open(hub: store::Hub, person_name: &str, channel: Option<&str>) -> Result<Self> {
        let person = agent_hub::person_handle_in(&hub.meta()?, person_name);
        let mut cache = HubReadCache::default();
        let snapshot = load_snapshot(&hub, &person, &person, &BTreeMap::new(), &mut cache)?;
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
            pinned_parent: None,
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
            pending_stop: None,
            thread: None,
            reply_to: None,
            search: None,
            show_help: false,
            focus_message: None,
            scroll_to_focus: false,
            is_bot_thinking: false,
            pinned: Vec::new(),
            show_pins: false,
            pins_selected: 0,
            hits: HitAreas::default(),
            cache,
            refresh_generation: next_refresh_generation(),
            refresh_in_flight: None,
            deferred_marks: None,
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

    /// True when the person looks at the DM with hub-bot.
    pub fn is_bot_dm(&self) -> bool {
        self.is_person_view()
            && matches!(self.selected_row(), Some(Row::Dm { peer, .. }) if peer == store::BOT_HANDLE)
    }

    /// Purpose: Read the hub again and keep the selected row when it still exists.
    /// Input: The browser state and the hub files.
    /// Output: Ok, or the read error. A row that is gone selects a valid fallback.
    pub fn reload(&mut self) -> Result<()> {
        let previous_index = self.selected_index(&self.rows()).unwrap_or(0);
        self.reload_snapshot()?;
        self.reselect(previous_index)
    }

    /// Purpose: Start a background refresh, the off-thread form of `reload`.
    /// Input: The browser state.
    /// Output: The job to run off the UI thread, or `None` while one runs.
    /// Note: The job takes the read cache and gives it back in its result.
    pub fn begin_refresh(&mut self) -> Option<HubRefreshJob> {
        if self.refresh_in_flight.is_some() {
            return None;
        }
        self.refresh_in_flight = Some(self.refresh_generation);
        Some(HubRefreshJob {
            generation: self.refresh_generation,
            hub: self.hub.clone(),
            person_name: self.person_name.clone(),
            person: self.person.clone(),
            perspective: self.perspective.clone(),
            unsaved_read: self.unsaved_read.clone(),
            conv: self.selected_row().and_then(|row| row.conv()),
            cache: std::mem::take(&mut self.cache),
        })
    }

    /// Purpose: Apply the result of a background refresh, as `reload` does.
    /// Input: The generation of the job and its result.
    /// Output: The read positions of the person for the caller to save off
    ///     the UI thread. `None` when the result is stale, because a local
    ///     load (a new row, perspective, thread, post or reload) ran after
    ///     the job started, or when the refresh failed.
    /// Note: This reads no hub file, unless the selected row is gone and the
    ///     fallback row shows another conversation.
    pub fn finish_refresh(
        &mut self,
        generation: u64,
        result: Result<HubRefresh, String>,
    ) -> Option<Vec<ReadMark>> {
        if self.refresh_in_flight == Some(generation) {
            self.refresh_in_flight = None;
        }
        if generation != self.refresh_generation {
            return None;
        }
        let refresh = match result {
            Ok(refresh) => refresh,
            Err(error) => {
                tracing::warn!(%error, "hub browser: refresh failed");
                return None;
            }
        };
        let previous_index = self.selected_index(&self.rows()).unwrap_or(0);
        if refresh.person != self.person {
            if self.is_person_view() {
                self.perspective = refresh.person.clone();
            }
            self.person = refresh.person;
        }
        for (conv, count) in refresh.saved_read {
            if self
                .unsaved_read
                .get(&conv)
                .is_some_and(|read| *read <= count)
            {
                self.unsaved_read.remove(&conv);
            }
        }
        self.snapshot = refresh.snapshot;
        self.cache = refresh.cache;
        self.deferred_marks = Some(Vec::new());
        let result = self.reselect(previous_index);
        let marks = self.deferred_marks.take().unwrap_or_default();
        if let Err(error) = result {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
        Some(marks)
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
                self.reply_to = None;
                self.pinned_parent = None;
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
        self.refresh_generation = next_refresh_generation();
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
        self.snapshot = load_snapshot(
            &self.hub,
            &self.perspective,
            &self.person,
            &read_floor,
            &mut self.cache,
        )?;
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
        if self.show_help {
            self.show_help = false;
            return HubKeyOutcome::Changed;
        }
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if self.is_composing {
            return self.handle_compose_key(key);
        }
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        if self.show_pins
            && let Some(outcome) = self.handle_pins_key(key)
        {
            return outcome;
        }
        let rows = self.rows();
        let pending_stop = self.pending_stop.take();
        match key.code {
            KeyCode::Char('g') => {
                if let Some(peer) = self.selected_agent_peer() {
                    return HubKeyOutcome::GoToMember(peer);
                }
            }
            KeyCode::Char('x') => {
                if let Some(peer) = self.selected_agent_peer() {
                    return self.request_stop(peer, pending_stop);
                }
            }
            KeyCode::Esc if self.reply_to.is_some() => {
                // Dropping the reply also drops it from the draft.
                self.reply_to = None;
                self.pinned_parent = None;
            }
            KeyCode::Esc if self.thread.is_some() => self.close_thread(),
            KeyCode::Esc | KeyCode::Char('q') => return HubKeyOutcome::Close,
            KeyCode::Char('/') => self.search = Some(HubSearch::default()),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char(']') => self.jump_unread(1),
            KeyCode::Char('[') => self.jump_unread(-1),
            KeyCode::Char('r') if self.thread.is_none() => {
                if let Some(last) = self.feed.last() {
                    self.start_reply(last.id.clone());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(&rows, -1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(&rows, 1),
            KeyCode::PageUp => self.feed_scroll = self.feed_scroll.saturating_add(PAGE_LINES),
            KeyCode::PageDown => self.feed_scroll = self.feed_scroll.saturating_sub(PAGE_LINES),
            KeyCode::End => self.feed_scroll = 0,
            KeyCode::Char('p') => self.open_picker(),
            KeyCode::Char('P') => self.toggle_pins(),
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
            self.sync_pinned_parent();
            return;
        }
        if let Some(search) = self.search.as_mut() {
            // The query is one line, so newlines become spaces.
            search.query.push_str(&text.replace(['\r', '\n'], " "));
            self.refresh_search();
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
                if self.picker.is_none()
                    && !self.show_help
                    && contains(self.hits.feed_scrollbar, col, row) =>
            {
                self.is_dragging_scrollbar = true;
                self.drag_feed_scrollbar(row);
            }
            // The reply chip lies over the feed, and a click on it is a button.
            // The help overlay lies over the feed, and a click only closes it.
            MouseEventKind::Down(MouseButton::Left)
                if self.picker.is_none()
                    && !self.show_help
                    && contains(self.hits.feed, col, row)
                    && !self.is_on_feed_button(col, row) =>
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
        if contains(hits.pin_drawer, col, row) {
            let is_row = hits.pin_rows.iter().any(|(y, _)| *y == row);
            return is_row.then_some(HoverKey::PinRow(row));
        }
        if contains(hits.pin_pill, col, row) {
            return Some(HoverKey::PinPill);
        }
        if hits
            .pin_chip
            .as_ref()
            .is_some_and(|(rect, _)| contains(*rect, col, row))
        {
            return Some(HoverKey::PinChip);
        }
        if contains(hits.header_go, col, row) {
            return Some(HoverKey::HeaderGo);
        }
        if contains(hits.thread_back, col, row) {
            return Some(HoverKey::ThreadBack);
        }
        if self.is_on_reply_chip(col, row) {
            return Some(HoverKey::ReplyChip);
        }
        if self.search_row_at(col, row).is_some() {
            return Some(HoverKey::SearchRow(row));
        }
        if contains(hits.header_stop, col, row) {
            return Some(HoverKey::HeaderStop);
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
        if let Some(span) = hits
            .feed_spans
            .iter()
            .find(|span| span.row == row && span.cols.contains(&col))
        {
            return Some(HoverKey::Span(span.row, span.cols.start));
        }
        if !contains(hits.feed, col, row) || row < hits.feed_top {
            return None;
        }
        let line = (row - hits.feed_top) as usize;
        let message = hits.feed_line_messages.get(line).copied().flatten()?;
        Some(HoverKey::FeedMessage(message))
    }

    /// The target of the clickable feed span at a screen cell.
    fn feed_span_at(&self, col: u16, row: u16) -> Option<SpanTarget> {
        self.hits
            .feed_spans
            .iter()
            .find(|span| span.row == row && span.cols.contains(&col))
            .map(|span| span.target.clone())
    }

    fn search_row_at(&self, col: u16, row: u16) -> Option<String> {
        self.search.as_ref()?;
        if !contains(self.hits.main, col, row) {
            return None;
        }
        self.hits
            .search_rows
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, id)| id.clone())
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
            SpanTarget::Thread(root) => self.open_thread(&root),
        }
        HubKeyOutcome::Changed
    }

    /// Purpose: Show the DM of the person with a member.
    /// Input: The member handle.
    /// Output: None. The view changes to the person when it shows another
    ///     member. The own handle and unknown handles change nothing.
    pub fn open_dm_with(&mut self, handle: &str) {
        let is_member = handle == store::BOT_HANDLE || self.snapshot.meta.member(handle).is_some();
        if handle == self.person || !is_member {
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

    /// Purpose: Apply one mouse-wheel scroll to the popup or the thread pane.
    /// Input: The scroll lines (positive is down) and the screen position.
    /// Output: None. The popup selection or the feed scroll changes in place.
    /// Note: The app sends wheel events here, not to `handle_mouse`. The scroll
    /// changes only when the pointer is over the thread pane and the picker
    /// is closed.
    pub fn handle_scroll(&mut self, lines: i32, col: u16, row: u16) {
        if lines == 0 || self.picker.is_some() {
            return;
        }
        if self.is_composing && self.composer.popup_contains(col, row) {
            let kind = if lines > 0 {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            };
            self.composer.handle_mouse(kind, col, row);
            return;
        }
        if !contains(self.hits.main, col, row) {
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

    /// Purpose: Show only the thread of a message in the selected conversation.
    /// Input: The id of the root or of any reply of the thread.
    /// Output: None. The thread of the root opens, also for a legacy reply
    ///     to a reply. A message in another conversation (an agent can reply
    ///     across conversations) opens no thread, and a notice says so,
    ///     because a reply here could not point at it.
    pub fn open_thread(&mut self, id: &str) {
        let root = self.thread_root_of(id);
        let root = root.as_str();
        self.stop_compose();
        let previous_thread = self.thread.clone();
        let previous_reply = self.reply_to.clone();
        let is_draft_elsewhere = self.is_draft_elsewhere();
        self.thread = Some(root.to_string());
        if !is_draft_elsewhere {
            self.reply_to = None;
        }
        self.feed_scroll = 0;
        self.feed_selection = None;
        if let Err(error) = self.load_feed(false) {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
        if !self.feed.iter().any(|m| m.id == root) {
            // Put the old state back, so a paused draft keeps its own parent
            // and never takes the rejected root.
            self.notice = Some("That message is in another conversation.".to_string());
            self.thread = previous_thread;
            self.reply_to = previous_reply;
            if let Err(error) = self.load_feed(false) {
                tracing::warn!(%error, "hub browser: feed load failed");
            }
        }
    }

    /// The thread root of message `id` in the shown conversation, or `id`
    /// itself when the conversation does not hold it.
    fn thread_root_of(&self, id: &str) -> String {
        let Some(conv) = self.selected_row().and_then(|row| row.conv()) else {
            return id.to_string();
        };
        match self.hub.messages(&conv, 0) {
            Ok(messages) => {
                store::resolve_thread_root(&messages, id).unwrap_or_else(|| id.to_string())
            }
            Err(error) => {
                tracing::warn!(%error, "hub browser: feed load failed");
                id.to_string()
            }
        }
    }

    fn close_thread(&mut self) {
        self.leave_thread();
        self.feed_scroll = 0;
        self.feed_selection = None;
        if let Err(error) = self.load_feed(false) {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
    }

    /// Purpose: Drop the thread filter, with no feed load.
    /// Output: None. A paused draft in the thread stays a reply to its root.
    fn leave_thread(&mut self) {
        let thread = self.thread.take();
        if !self.composer.is_empty() && self.compose_target.is_some() && !self.is_draft_elsewhere()
        {
            self.reply_to = thread.or_else(|| self.reply_to.take());
        }
    }

    /// Purpose: Select the next or previous conversation with unread messages.
    /// Input: The step, 1 for down or -1 for up.
    /// Output: None. The search wraps around the sidebar. No unread changes nothing.
    fn jump_unread(&mut self, step: isize) {
        let rows = self.rows();
        let len = rows.len() as isize;
        let start = self.selected_index(&rows).unwrap_or(0) as isize;
        for offset in 1..=len {
            let idx = (start + step * offset).rem_euclid(len) as usize;
            let has_unread = rows[idx]
                .conv()
                .is_some_and(|conv| self.snapshot.unread(&conv) > 0);
            if has_unread {
                self.stop_compose();
                // The unread message can be outside the open thread of the
                // same conversation, so the jump always shows it whole.
                self.leave_thread();
                self.select(Some(rows[idx].clone()));
                return;
            }
        }
        self.notice = Some("No unread conversations.".to_string());
    }

    /// Purpose: Edit the search query or move in the results.
    /// Input: A key event while the search is open.
    /// Output: The next browser action. Enter jumps to the selected result.
    fn handle_search_key(&mut self, key: &KeyEvent) -> HubKeyOutcome {
        let Some(search) = self.search.as_mut() else {
            return HubKeyOutcome::Changed;
        };
        let last = search.results.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                // The conversation shows again, so its new messages are read now.
                self.search = None;
                if let Err(error) = self.load_feed(false) {
                    tracing::warn!(%error, "hub browser: feed load failed");
                }
            }
            KeyCode::Up => search.selected = search.selected.saturating_sub(1),
            KeyCode::Down => search.selected = (search.selected + 1).min(last),
            KeyCode::Enter => {
                if let Some(id) = search.results.get(search.selected).map(|m| m.id.clone()) {
                    self.jump_to_message(&id);
                }
            }
            KeyCode::Backspace => {
                search.query.pop();
                self.refresh_search();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                search.query.push(c);
                self.refresh_search();
            }
            _ => {}
        }
        HubKeyOutcome::Changed
    }

    /// Run the search query again over the whole hub.
    fn refresh_search(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        search.selected = 0;
        search.results = match store::search(&self.hub, &search.query, SEARCH_LIMIT) {
            Ok(results) => results,
            Err(error) => {
                tracing::warn!(%error, "hub browser: search failed");
                Vec::new()
            }
        };
    }

    /// Purpose: Close the search and show a found message in its conversation.
    /// Input: The message id.
    /// Output: None. A channel opens as is. A DM opens from the view of the
    ///     person when the person is in it, or else from one of its members.
    fn jump_to_message(&mut self, id: &str) {
        let Some(message) = self
            .search
            .take()
            .and_then(|search| search.results.into_iter().find(|m| m.id == id))
        else {
            return;
        };
        // A thread filter would hide a result from the same conversation.
        if self.selected_row().and_then(|row| row.conv()).as_deref() == Some(message.conv.as_str())
        {
            self.leave_thread();
        }
        let row = match store::split_dm_conv(&message.conv) {
            None => message
                .conv
                .strip_prefix('#')
                .map(|name| Row::Channel(name.to_string())),
            Some((a, b)) => {
                let viewer = if [a, b].contains(&self.person.as_str()) {
                    self.person.clone()
                } else {
                    a.to_string()
                };
                let peer = if viewer == a { b } else { a };
                // Select the DM before the feed loads, so no other
                // conversation is marked read.
                if let Err(error) = self.show_perspective(&viewer, Some(message.conv.clone())) {
                    self.notice = Some(format!("Could not load @{viewer}: {error}"));
                }
                Some(Row::Dm {
                    peer: peer.to_string(),
                    conv: message.conv.clone(),
                })
            }
        };
        let Some(row) = row else {
            return;
        };
        self.select(Some(row));
        let is_loaded = self.feed.iter().any(|m| m.id == message.id);
        self.focus_message = Some(message.id);
        self.scroll_to_focus = true;
        // The message is older than the feed window, so load a window with it.
        if !is_loaded && let Err(error) = self.load_feed(false) {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
    }

    /// Open or close the pinned-messages drawer of the selected channel.
    fn toggle_pins(&mut self) {
        if !matches!(self.selected_row(), Some(Row::Channel(_))) {
            self.notice = Some("Only channels have pinned messages.".to_string());
            return;
        }
        self.show_pins = !self.show_pins;
        self.pins_selected = 0;
    }

    /// Purpose: Apply one key press to the open pinned-messages drawer.
    /// Input: The key event.
    /// Output: The outcome of a drawer key. `None` for another key, which
    ///     closes the drawer and then works as usual.
    fn handle_pins_key(&mut self, key: &KeyEvent) -> Option<HubKeyOutcome> {
        let last = self.pinned.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('P') => self.show_pins = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.pins_selected = self.pins_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.pins_selected = (self.pins_selected + 1).min(last);
            }
            KeyCode::Enter => {
                if let Some(id) = self.pinned.get(self.pins_selected).map(|m| m.id.clone()) {
                    self.jump_to_pin(&id);
                }
            }
            KeyCode::Char('u') => {
                let id = self.pinned.get(self.pins_selected).map(|m| m.id.clone())?;
                return Some(self.pin_outcome(&id).unwrap_or(HubKeyOutcome::Changed));
            }
            _ => {
                self.show_pins = false;
                return None;
            }
        }
        Some(HubKeyOutcome::Changed)
    }

    /// Purpose: Close the drawer and show a pinned message in the feed.
    /// Input: The message id.
    /// Output: None. An old message outside the feed window loads with some
    ///     context, and a thread of the channel closes.
    fn jump_to_pin(&mut self, id: &str) {
        self.show_pins = false;
        self.leave_thread();
        self.focus_message = Some(id.to_string());
        self.scroll_to_focus = true;
        if let Err(error) = self.load_feed(false) {
            tracing::warn!(%error, "hub browser: feed load failed");
        }
    }

    /// Purpose: Ask the caller to pin or unpin a message of the shown channel.
    /// Input: The message id.
    /// Output: `Unpin` for a pinned message, `Pin` for another one, or `None`
    ///     outside the person view or outside a channel, which are read only.
    fn pin_outcome(&self, message_id: &str) -> Option<HubKeyOutcome> {
        let Some(Row::Channel(name)) = self.selected_row() else {
            return None;
        };
        if !self.is_person_view() {
            return None;
        }
        let channel = format!("#{name}");
        let message_id = message_id.to_string();
        if self.pinned.iter().any(|m| m.id == message_id) {
            Some(HubKeyOutcome::Unpin {
                channel,
                message_id,
            })
        } else {
            Some(HubKeyOutcome::Pin {
                channel,
                message_id,
            })
        }
    }

    /// Purpose: Apply a click while the drawer is open.
    /// Input: The screen position.
    /// Output: The outcome of a click on the drawer. `None` for a click
    ///     outside it, which closes the drawer and then works as usual.
    fn click_pins(&mut self, col: u16, row: u16) -> Option<HubKeyOutcome> {
        let unpin = self
            .hits
            .pin_unpins
            .iter()
            .find(|(rect, _)| contains(*rect, col, row))
            .map(|(_, id)| id.clone());
        if let Some(id) = unpin {
            return Some(self.pin_outcome(&id).unwrap_or(HubKeyOutcome::Changed));
        }
        let hit = self
            .hits
            .pin_rows
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, id)| id.clone());
        if contains(self.hits.pin_drawer, col, row) {
            if let Some(id) = hit {
                self.jump_to_pin(&id);
            }
            return Some(HubKeyOutcome::Changed);
        }
        if contains(self.hits.pin_pill, col, row) {
            self.show_pins = false;
            return Some(HubKeyOutcome::Changed);
        }
        self.show_pins = false;
        None
    }

    /// True on a button over the feed: the reply chip, the pin chip, or the drawer.
    fn is_on_feed_button(&self, col: u16, row: u16) -> bool {
        let is_on_pin_chip = self
            .hits
            .pin_chip
            .as_ref()
            .is_some_and(|(rect, _)| contains(*rect, col, row));
        self.is_on_reply_chip(col, row)
            || is_on_pin_chip
            || contains(self.hits.pin_drawer, col, row)
    }

    /// The peer of the selected DM when it is a live agent, the target of
    /// the member actions.
    fn selected_agent_peer(&self) -> Option<String> {
        let Some(Row::Dm { peer, .. }) = self.selected_row() else {
            return None;
        };
        let member = self.snapshot.meta.member(&peer)?;
        (member.kind == MemberKind::Agent && member.active).then_some(peer)
    }

    /// Purpose: Stop a member on the second request, so one slip cannot stop it.
    /// Input: The member handle and the member of the last request, if any.
    /// Output: `StopMember` on the second request in a row. Otherwise a notice.
    fn request_stop(&mut self, peer: String, pending: Option<String>) -> HubKeyOutcome {
        if pending.as_deref() == Some(peer.as_str()) {
            self.notice = None;
            return HubKeyOutcome::StopMember(peer);
        }
        self.notice = Some(format!("Press x again to stop @{peer}."));
        self.pending_stop = Some(peer);
        HubKeyOutcome::Changed
    }

    fn is_on_reply_chip(&self, col: u16, row: u16) -> bool {
        self.hits
            .reply_chip
            .as_ref()
            .is_some_and(|(rect, _)| contains(*rect, col, row))
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
        if self.show_help {
            self.show_help = false;
            return HubKeyOutcome::Changed;
        }
        // The open picker lies over the search results and gets the click.
        if self.picker.is_some() {
            self.click_area(col, row);
            return HubKeyOutcome::Changed;
        }
        if let Some(id) = self.search_row_at(col, row) {
            self.jump_to_message(&id);
            return HubKeyOutcome::Changed;
        }
        if self.show_pins
            && let Some(outcome) = self.click_pins(col, row)
        {
            return outcome;
        }
        if contains(self.hits.pin_pill, col, row) {
            self.toggle_pins();
            return HubKeyOutcome::Changed;
        }
        if let Some((rect, id)) = self.hits.pin_chip.clone()
            && contains(rect, col, row)
        {
            return self.pin_outcome(&id).unwrap_or(HubKeyOutcome::Changed);
        }
        if let Some((rect, id)) = self.hits.reply_chip.clone()
            && contains(rect, col, row)
        {
            self.start_reply(id);
            return HubKeyOutcome::Changed;
        }
        if contains(self.hits.thread_back, col, row) {
            self.close_thread();
            return HubKeyOutcome::Changed;
        }
        let pending_stop = self.pending_stop.take();
        if self.picker.is_none()
            && let Some(peer) = self.selected_agent_peer()
        {
            if contains(self.hits.header_go, col, row) {
                return HubKeyOutcome::GoToMember(peer);
            }
            if contains(self.hits.header_stop, col, row) {
                return self.request_stop(peer, pending_stop);
            }
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

    /// True when a paused draft is pinned to another conversation than the
    /// shown one. Then `reply_to` is the parent of that draft.
    fn is_draft_elsewhere(&self) -> bool {
        let shown = self.selected_row().as_ref().and_then(post_target);
        !self.composer.is_empty()
            && self
                .compose_target
                .as_ref()
                .is_some_and(|pinned| Some(pinned) != shown.as_ref())
    }

    /// The parent of the draft: the parent fixed when a nonempty draft
    /// started, or else the parent that the shown view implies.
    fn draft_parent(&self) -> Option<String> {
        match &self.pinned_parent {
            Some(parent) if !self.composer.is_empty() => parent.clone(),
            _ => self.live_parent(),
        }
    }

    /// Fix the parent when the draft gets its first text, and release it
    /// when the draft is empty again.
    fn sync_pinned_parent(&mut self) {
        if self.composer.is_empty() {
            self.pinned_parent = None;
        } else if self.pinned_parent.is_none() {
            self.pinned_parent = Some(self.live_parent());
        }
    }

    /// The parent that the shown view implies: the parent of a draft for
    /// another conversation, or else the open thread or the chosen reply.
    fn live_parent(&self) -> Option<String> {
        if self.is_draft_elsewhere() {
            return self.reply_to.clone();
        }
        self.thread.clone().or_else(|| self.reply_to.clone())
    }

    /// Leave the post box. An empty draft releases its destination.
    fn stop_compose(&mut self) {
        self.is_composing = false;
        if self.composer.is_empty() {
            self.compose_target = None;
        }
    }

    /// Purpose: Start a reply to a message of the shown conversation.
    /// Input: The parent message id.
    /// Output: None. The draft replies to the thread root of the parent, so
    ///     threads stay flat. A paused draft for another conversation keeps
    ///     its destination and gets no parent, because the hub refuses a
    ///     reply to a message of another conversation. A notice says why.
    fn start_reply(&mut self, parent_id: String) {
        let shown = self.selected_row().as_ref().and_then(post_target);
        if let Some(pinned) = self.compose_target.clone()
            && !self.composer.is_empty()
            && Some(&pinned) != shown.as_ref()
        {
            self.notice = Some(format!("Send or clear the draft to {pinned} first."));
            return;
        }
        let parent_id = self.thread_root_of(&parent_id);
        // An explicit reply changes the parent of a draft that has text.
        if !self.composer.is_empty() {
            self.pinned_parent = Some(Some(parent_id.clone()));
        }
        self.reply_to = Some(parent_id);
        self.start_compose();
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
                    return match self.draft_parent() {
                        Some(reply_to) => HubKeyOutcome::Reply { to, text, reply_to },
                        None => HubKeyOutcome::Post { to, text },
                    };
                }
            }
            ComposerKey::Edited => self.sync_pinned_parent(),
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
        if row != self.selected {
            // A paused draft keeps its parent in `reply_to`: the thread root or
            // the reply of the shown conversation. A draft that is already
            // pinned elsewhere keeps the parent that it has.
            let thread = self.thread.take();
            let has_paused_draft = !self.composer.is_empty() && self.compose_target.is_some();
            if !has_paused_draft {
                self.reply_to = None;
            } else if !self.is_draft_elsewhere() {
                self.reply_to = thread.or_else(|| self.reply_to.take());
            }
            self.focus_message = None;
            self.show_pins = false;
        }
        self.selected = row;
        self.feed_scroll = 0;
        self.feed_selection = None;
        self.load_feed(true)
    }

    /// Purpose: Load the feed of the selected row.
    /// Input: Whether to place the unread marker again.
    /// Output: Ok, or the read error. In the person view, the read cursor moves to the end.
    fn load_feed(&mut self, is_new_selection: bool) -> Result<()> {
        self.refresh_generation = next_refresh_generation();
        let Some(conv) = self.selected_row().and_then(|row| row.conv()) else {
            self.feed.clear();
            self.first_unread = None;
            return Ok(());
        };
        let loaded = match self.cache.feed.take() {
            // The background refresh read this file a moment ago.
            Some(feed) if feed.conv == conv && self.deferred_marks.is_some() => feed,
            cached => load_conv(&self.hub, &conv, cached)?,
        };
        let result = self.show_feed(&conv, &loaded, is_new_selection);
        self.cache.feed = Some(loaded);
        result
    }

    /// Purpose: Show the loaded messages of the selected conversation.
    /// Input: The conversation, all of its messages, and whether to place the
    ///     unread marker again.
    /// Output: Ok, or the cursor write error. In the person view, the read
    ///     cursor moves to the end.
    fn show_feed(&mut self, conv: &str, loaded: &FeedTail, is_new_selection: bool) -> Result<()> {
        let total = loaded.messages.len();
        debug_assert_eq!(total, loaded.tail.message_count);
        let channel = conv
            .strip_prefix('#')
            .and_then(|name| self.snapshot.meta.channel(name));
        self.pinned = match channel {
            Some(channel) => store::resolve_pins(&channel.pins, &loaded.messages)
                .into_iter()
                .map(|(_, message)| message)
                .collect(),
            None => Vec::new(),
        };
        self.pins_selected = self.pins_selected.min(self.pinned.len().saturating_sub(1));
        let thread_messages: Vec<store::Message>;
        let messages: &[store::Message] = match &self.thread {
            Some(root) => {
                // Resolve legacy nested replies before the window limit.
                let members: BTreeSet<String> = store::thread_root_index(&loaded.messages)
                    .into_iter()
                    .filter(|(_, thread_root)| *thread_root == root.as_str())
                    .map(|(id, _)| id.to_string())
                    .collect();
                thread_messages = loaded
                    .messages
                    .iter()
                    .filter(|m| members.contains(&m.id))
                    .cloned()
                    .collect();
                &thread_messages
            }
            None => &loaded.messages,
        };
        let skip = feed_window_start(messages, self.focus_message.as_deref());
        let mut feed = messages[skip..].to_vec();
        // A thread longer than the window still shows its root first.
        if let Some(root) = &self.thread
            && !feed.iter().any(|m| m.id == *root)
            && let Some(root) = messages[..skip].iter().find(|m| m.id == *root)
        {
            feed.insert(0, root.clone());
        }
        // A drop or replacement can move text under the stored line indices.
        if self.feed_selection.is_some() && !feed.starts_with(&self.feed) {
            self.feed_selection = None;
        }
        self.feed = feed;
        if self.thread.is_some() {
            // Feed indices of a thread are not conversation positions.
            self.first_unread = None;
        } else if is_new_selection {
            let read = self.snapshot.read.get(conv).copied().unwrap_or(0);
            self.first_unread = self
                .feed
                .iter()
                .enumerate()
                .skip(read.saturating_sub(skip))
                .find(|(_, message)| message.from != self.perspective)
                .map(|(idx, _)| idx);
        }
        // A thread shows only some messages, so the conversation stays unread.
        // A thread or a search shows only part or none of the conversation,
        // so the conversation stays unread.
        if self.is_person_view() && self.thread.is_none() && self.search.is_none() {
            let count = total;
            if is_person_member(&self.snapshot.meta, &self.person) {
                self.save_read(conv, count, Some(loaded.tail.offset()))?;
            } else {
                self.unsaved_read.insert(conv.to_string(), count);
            }
            self.snapshot.read.insert(conv.to_string(), count);
            self.snapshot.unread_counts.insert(conv.to_string(), 0);
        }
        Ok(())
    }

    /// Purpose: Save a read position of the person.
    /// Input: The conversation, the message count, and its byte offset.
    /// Output: Ok, or the write error. While a background refresh applies,
    ///     a forward move waits in `deferred_marks` and nothing is written.
    fn save_read(
        &mut self,
        conv: &str,
        count: usize,
        offset: Option<store::LineOffset>,
    ) -> Result<()> {
        let Some(marks) = self.deferred_marks.as_mut() else {
            return agent_hub::mark_read_at(&self.hub, &self.person, conv, count, offset);
        };
        if self
            .snapshot
            .read
            .get(conv)
            .is_none_or(|read| *read < count)
        {
            marks.push(ReadMark {
                conv: conv.to_string(),
                count,
                offset,
            });
        }
        Ok(())
    }
}

/// Purpose: Read every message of one conversation, from the cache when it can.
/// Input: The hub, the conversation, and the cached feed, if any.
/// Output: The cache for `conv`, with the appended messages added. A cache
///     of another conversation, or of a shorter or replaced file, is read
///     again from line 0.
fn load_conv(hub: &store::Hub, conv: &str, cached: Option<FeedTail>) -> Result<FeedTail> {
    let mut feed = cached
        .filter(|feed| feed.conv == conv)
        .unwrap_or_else(|| FeedTail {
            conv: conv.to_string(),
            tail: store::ConvTail::default(),
            messages: Vec::new(),
        });
    match hub.read_tail(conv, &mut feed.tail)? {
        store::TailRead::Unchanged => {}
        store::TailRead::Appended(messages) => feed.messages.extend(messages),
        store::TailRead::Reset(messages) => feed.messages = messages,
    }
    Ok(feed)
}

/// Purpose: Count the unread messages of one conversation for a member.
/// Input: The hub, the conversation, the member, its read count, the byte
///     offset of that count when it is known, and the cached state.
/// Output: The state with the unread count, or `None` when the file has
///     fewer messages than the read count (then nothing is unread).
/// Method:
///     1. With a cache for the same read count, read only the new bytes.
///     2. If not, read from the read cursor, with a seek when the offset
///        is known.
fn unread_tail(
    hub: &store::Hub,
    conv: &str,
    perspective: &str,
    read: usize,
    read_offset: Option<store::LineOffset>,
    cached: Option<UnreadTail>,
) -> Result<Option<UnreadTail>> {
    let from_others = |messages: &[store::Message]| {
        messages
            .iter()
            .filter(|message| message.from != perspective)
            .count()
    };
    if let Some(mut state) = cached.filter(|state| state.read == read) {
        match hub.read_tail(conv, &mut state.tail)? {
            store::TailRead::Unchanged => {}
            store::TailRead::Appended(messages) => state.unread += from_others(&messages),
            store::TailRead::Reset(messages) if messages.len() < read => return Ok(None),
            store::TailRead::Reset(messages) => state.unread = from_others(&messages[read..]),
        }
        return Ok(Some(state));
    }
    let slice = hub.messages_from_offset(conv, read, read_offset, None)?;
    Ok(slice.tail(read).map(|tail| UnreadTail {
        read,
        tail,
        unread: from_others(&slice.messages),
    }))
}

/// Purpose: Find the first message that the feed shows.
/// Input: The messages of the conversation, oldest first, and the focused
///     message id, if any.
/// Output: The index of the first of the last FEED_LIMIT messages. A focused
///     message before that window moves the start back to show it, with a
///     few messages of context.
fn feed_window_start(messages: &[store::Message], focus: Option<&str>) -> usize {
    let skip = messages.len().saturating_sub(FEED_LIMIT);
    let focus_idx = focus.and_then(|id| messages.iter().position(|m| m.id == id));
    match focus_idx {
        Some(idx) => skip.min(idx.saturating_sub(FOCUS_CONTEXT)),
        None => skip,
    }
}

/// Purpose: Read the hub as one member sees it.
/// Input: The hub, the perspective handle, the person handle, read
///     positions that the cursor file does not have yet, and the read cache.
/// Output: A snapshot with a count for every channel and every DM row.
/// Note: The person sees every member as a DM row. Another member sees only
///     the DMs that it is in. The unread counts read only the bytes after
///     the cache or after the read cursor. The cache of another perspective
///     is dropped.
pub fn load_snapshot(
    hub: &store::Hub,
    perspective: &str,
    person: &str,
    read_floor: &BTreeMap<String, usize>,
    cache: &mut HubReadCache,
) -> Result<HubSnapshot> {
    let mut meta = hub.meta()?;
    for member in &mut meta.members {
        member.active = member.is_live();
    }
    let cursors = hub.cursors(perspective)?;
    let mut read = cursors.read.clone();
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
    if cache.perspective != perspective {
        cache.perspective = perspective.to_string();
        cache.unread.clear();
    }
    let mut cached = std::mem::take(&mut cache.unread);
    let mut unread_counts = BTreeMap::new();
    for conv in convs {
        let start = read.get(&conv).copied().unwrap_or(0);
        // A read floor above the cursor has no saved byte offset.
        let is_cursor = cursors.read.get(&conv).copied().unwrap_or(0) == start;
        let offset = is_cursor.then(|| cursors.read_offset(&conv)).flatten();
        let state = unread_tail(hub, &conv, perspective, start, offset, cached.remove(&conv))?;
        unread_counts.insert(conv.clone(), state.as_ref().map_or(0, |state| state.unread));
        if let Some(state) = state {
            cache.unread.insert(conv, state);
        }
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
    // hub-bot is always there for the person, at the top of the DMs.
    dm_peers.retain(|peer| peer != store::BOT_HANDLE);
    if perspective == person {
        dm_peers.insert(0, store::BOT_HANDLE.to_string());
    }
    Ok(HubSnapshot {
        tokens: hub.tokens(&meta)?,
        meta,
        dm_peers,
        unread_counts,
        read,
        presences: hub.presences(),
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
    peer == store::BOT_HANDLE
        || snapshot
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
    /// An agent name: the color slot and the full handle. The handle is the
    /// click target, because a narrow feed can clip the shown name.
    Agent(usize, String),
    /// A person name, with the full handle.
    Person(String),
    Time,
    Text,
    Mention,
    /// A URL or a file path in message text. A wrapped link has one span
    /// per line, and each span keeps the target of the full link.
    Link(LinkTarget),
    /// The quote of a parent message above a reply. A click opens the thread.
    Quote,
    /// The "N replies" link under a message. A click opens the thread.
    Thread,
    /// The badge of a pinned message.
    Pin,
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

/// Whether the feed shows a whole conversation or one thread of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedMode {
    Conversation,
    Thread,
}

/// Feed lines and the feed index of the message on each line.
struct FeedLayout {
    lines: Vec<FeedLine>,
    /// One entry per line. Rules and gaps have none.
    messages: Vec<Option<usize>>,
}

/// Purpose: Lay out feed messages as chat lines, oldest first.
/// Input: The messages, the width, the index of the first unread message,
///     the names to highlight, the local date of today, the feed mode, and
///     the ids of the pinned messages.
/// Output: Lines of (text, part) spans and the message of each line. No line
///     is wider than `width`.
/// Method:
///     1. Put a day rule before the first message of each day.
///     2. Put a "new" rule before the first unread message. If a day rule is
///        already there, end the day rule with the "new" label.
///     3. Start a group with a sender and time header when the sender changes,
///        after a gap of GROUP_GAP_MS, after a rule, or at a pinned message.
///        The header of a pinned message ends with a pin badge.
///     4. In a conversation, put a quote of the parent above a reply.
///     5. Indent and wrap each message body under its group header.
///     6. In a conversation, put a "N replies" link under a message with replies.
fn feed_layout(
    messages: &[store::Message],
    width: usize,
    first_unread: Option<usize>,
    names: &FeedNames<'_>,
    today: NaiveDate,
    mode: FeedMode,
    pinned: &BTreeSet<String>,
) -> FeedLayout {
    let mut layout = FeedLayout {
        lines: Vec::new(),
        messages: Vec::new(),
    };
    if width < BODY_INDENT + 4 {
        return layout;
    }
    let reply_counts = reply_counts(messages);
    let mut prev: Option<&store::Message> = None;
    for (idx, message) in messages.iter().enumerate() {
        let time = local_time(message.ts_unix_ms);
        let is_new_day =
            prev.is_none_or(|p| local_time(p.ts_unix_ms).date_naive() != time.date_naive());
        let is_first_unread = first_unread == Some(idx);
        let lines = &mut layout.lines;
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
        let is_reply = mode == FeedMode::Conversation && message.reply_to.is_some();
        let is_pinned = pinned.contains(&message.id);
        let continues_group = !is_new_day
            && !is_first_unread
            && !is_reply
            && !is_pinned
            && prev.is_some_and(|p| {
                p.from == message.from
                    && message.ts_unix_ms.saturating_sub(p.ts_unix_ms) < GROUP_GAP_MS
            });
        if !continues_group && !is_new_day && !is_first_unread {
            lines.push(Vec::new());
        }
        layout.messages.resize(lines.len(), None);
        if !continues_group {
            let mut header = sender_header(message, &time, width, names);
            if is_pinned {
                push_pin_badge(&mut header, width);
            }
            lines.push(header);
        }
        if is_reply {
            lines.push(quote_line(messages, message, width));
        }
        push_body(lines, message, width, names);
        let replies = reply_counts.get(message.id.as_str()).copied().unwrap_or(0);
        if mode == FeedMode::Conversation && replies > 0 {
            let label = if replies == 1 {
                "↩ 1 reply".to_string()
            } else {
                format!("↩ {replies} replies")
            };
            lines.push(vec![
                (" ".repeat(BODY_INDENT), Part::Text),
                (label, Part::Thread),
            ]);
        }
        layout.messages.resize(lines.len(), Some(idx));
        prev = Some(message);
    }
    layout
}

/// The layout lines alone, for tests of the line content.
#[cfg(test)]
fn feed_lines(
    messages: &[store::Message],
    width: usize,
    first_unread: Option<usize>,
    names: &FeedNames<'_>,
    today: NaiveDate,
) -> Vec<FeedLine> {
    feed_layout(
        messages,
        width,
        first_unread,
        names,
        today,
        FeedMode::Conversation,
        &BTreeSet::new(),
    )
    .lines
}

/// End a sender header with the pin badge when the line has room for it.
fn push_pin_badge(header: &mut FeedLine, width: usize) {
    const BADGE: &str = "  📌 pinned";
    let used: usize = header.iter().map(|(text, _)| text.width()).sum();
    if used + BADGE.width() <= width {
        header.push((BADGE.to_string(), Part::Pin));
    }
}

/// Replies per thread root, over the feed. A legacy reply to a reply counts
/// for the root of its thread.
fn reply_counts(messages: &[store::Message]) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for (id, root) in store::thread_root_index(messages) {
        if id != root {
            *counts.entry(root).or_default() += 1;
        }
    }
    counts
}

/// A muted line that quotes the start of the parent of a reply.
fn quote_line(messages: &[store::Message], reply: &store::Message, width: usize) -> FeedLine {
    let parent = reply
        .reply_to
        .as_deref()
        .and_then(|id| messages.iter().find(|m| m.id == id));
    let quote = match parent {
        Some(parent) => {
            let first = parent.text.lines().next().unwrap_or_default();
            format!("↳ @{}: {first}", parent.from)
        }
        None => "↳ reply to an older message".to_string(),
    };
    let fitted: String = textwrap::wrap(&quote, width - BODY_INDENT)
        .first()
        .map(|line| line.to_string())
        .unwrap_or_default();
    vec![(" ".repeat(BODY_INDENT), Part::Text), (fitted, Part::Quote)]
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
        Part::Person(message.from.clone())
    } else {
        Part::Agent(name_color_slot(&message.from), message.from.clone())
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
        Part::Agent(_, handle) | Part::Person(handle) => {
            meta.member(handle)?;
            Some(SpanTarget::Member(handle.clone()))
        }
        Part::Mention => {
            if let Some(name) = text.strip_prefix('#') {
                return Some(SpanTarget::Channel(name.to_string()));
            }
            let handle = text.strip_prefix('@')?;
            if handle != store::BOT_HANDLE {
                meta.member(handle)?;
            }
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
    if state.show_help {
        render_help(buf, area, &theme);
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
    let tokens: u64 = state.snapshot.tokens.values().sum();
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
            let member_state = member_state(snapshot, peer, store::now_ms(), theme);
            let word = if *peer == state.person {
                "you".to_string()
            } else {
                member_state.word
            };
            (
                member_state.glyph.to_string(),
                Style::default().fg(member_state.color).bg(bg),
                peer.clone(),
                word,
            )
        }
    };
    let unread = row.conv().map(|conv| snapshot.unread(&conv)).unwrap_or(0);
    let is_dim = match row {
        Row::Dm { peer, .. } => !is_active_peer(snapshot, peer),
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
        let needs_you = matches!(row, Row::Dm { peer, .. }
            if snapshot.presences.get(peer).is_some_and(|p| p.activity == store::Activity::NeedsInput)
                && snapshot.meta.member(peer).is_some_and(|m| m.active));
        let fg = if needs_you {
            theme.accent_error
        } else {
            theme.gray
        };
        (right, Style::default().fg(fg).bg(bg))
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
    if state.search.is_some() {
        render_search(buf, inner, state, theme);
        return;
    }
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
    if state.show_pins {
        render_pins_drawer(buf, feed, state, theme);
    }
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
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let y = area.y + 1;
    let full = area;
    let buttons_x = if state.thread.is_some() {
        render_thread_back(buf, area, state, theme)
    } else {
        render_member_buttons(buf, area, state, theme)
    };
    let buttons_x = render_pin_pill(buf, area, buttons_x, state, theme);
    let area = Rect {
        width: buttons_x.saturating_sub(area.x),
        ..area
    };
    let title_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD);
    let (title, detail) = match row {
        Row::Section(_) => (String::new(), String::new()),
        _ if state.thread.is_some() => {
            let place = post_target(row).unwrap_or_default();
            let replies = state.feed.len().saturating_sub(1);
            (
                "Thread".to_string(),
                format!("in {place} · {}", plural(replies, "reply")),
            )
        }
        Row::Channel(name) => {
            let meta = &state.snapshot.meta;
            let mut detail = meta
                .channels
                .iter()
                .find(|c| &c.name == name)
                .map(|c| c.purpose.clone())
                .unwrap_or_default();
            let tokens = state
                .snapshot
                .tokens
                .get(&format!("#{name}"))
                .copied()
                .unwrap_or(0);
            if tokens > 0 {
                if !detail.is_empty() {
                    detail.push_str(" · ");
                }
                detail.push_str(&format!("~{tokens} tokens to agents"));
            }
            (format!("# {name}"), detail)
        }
        Row::Dm { peer, .. } => {
            let status = member_summary(&state.snapshot, peer, theme);
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
        full.x,
        y + 1,
        &"─".repeat(full.width as usize),
        Style::default().fg(theme.gray_dim),
        full.right(),
    );
}

/// Purpose: Draw the `[📌 N]` pill of a channel header that has pins.
/// Input: The buffer, the header area, the x where the right buttons start,
///     the state, and the theme.
/// Output: The x of the pill, or `right_x` when no pill is drawn. A click on
///     the pill opens the pinned-messages drawer.
fn render_pin_pill(
    buf: &mut Buffer,
    area: Rect,
    right_x: u16,
    state: &mut HubBrowserState,
    theme: &Theme,
) -> u16 {
    let is_channel = matches!(state.selected_row(), Some(Row::Channel(_)));
    if !is_channel || state.thread.is_some() || state.pinned.is_empty() {
        return right_x;
    }
    let label = format!("[📌 {}]", state.pinned.len());
    let width = label.width() as u16;
    let x = right_x.saturating_sub(width);
    if x <= area.x + 10 {
        return right_x;
    }
    let mut style = Style::default().fg(theme.accent_verify);
    if state.show_pins || state.hover == Some(HoverKey::PinPill) {
        style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    }
    span_at(buf, x, area.y + 1, &label, style, right_x);
    state.hits.pin_pill = Rect::new(x, area.y + 1, width, 1);
    x.saturating_sub(1)
}

/// Draw the "back" button of a thread header. Return the x of the button.
fn render_thread_back(
    buf: &mut Buffer,
    area: Rect,
    state: &mut HubBrowserState,
    theme: &Theme,
) -> u16 {
    let label = "[← back]";
    let width = label.width() as u16;
    let x = area.right().saturating_sub(width);
    let mut style = Style::default().fg(theme.command);
    if state.hover == Some(HoverKey::ThreadBack) {
        style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    }
    span_at(buf, x, area.y + 1, label, style, area.right());
    state.hits.thread_back = Rect::new(x, area.y + 1, width, 1);
    x.saturating_sub(1)
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
    mentions.insert(format!("@{}", store::BOT_HANDLE));
    mentions.extend(meta.channels.iter().map(|c| format!("#{}", c.name)));
    let names = FeedNames {
        persons: &persons,
        mentions: &mentions,
    };
    let today = Local::now().date_naive();
    let mode = match state.thread {
        Some(_) => FeedMode::Thread,
        None => FeedMode::Conversation,
    };
    let pinned: BTreeSet<String> = state.pinned.iter().map(|m| m.id.clone()).collect();
    let FeedLayout {
        lines,
        messages: line_messages,
    } = feed_layout(
        &state.feed,
        area.width as usize,
        state.first_unread,
        &names,
        today,
        mode,
        &pinned,
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
    let focused = state
        .focus_message
        .as_deref()
        .and_then(|id| state.feed.iter().position(|m| m.id == id));
    if std::mem::take(&mut state.scroll_to_focus)
        && let Some(line) =
            focused.and_then(|idx| line_messages.iter().position(|m| *m == Some(idx)))
    {
        // Put the found message a third of the way down the feed.
        state.feed_scroll = lines.len().saturating_sub(line + height * 2 / 3);
    }
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
        let message = line_messages[start + i].map(|idx| &state.feed[idx]);
        if focused.is_some() && line_messages[start + i] == focused {
            fill(
                buf,
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                Style::default().bg(theme.bg_highlight),
            );
        }
        for (text, part) in line {
            let target = match (part, message) {
                (Part::Thread, Some(message)) => Some(SpanTarget::Thread(message.id.clone())),
                (Part::Quote, Some(message)) => message.reply_to.clone().map(SpanTarget::Thread),
                _ => span_target(text, part, &state.snapshot.meta),
            };
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
    state.hits.feed_line_messages = line_messages[start..end].to_vec();
    render_reply_chip(buf, area, top, state, theme);
    render_member_tooltip(buf, area, state, theme);
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
        Part::Agent(slot, _) => Style::default()
            .fg(palette[slot % palette.len()])
            .add_modifier(Modifier::BOLD),
        Part::Person(_) => Style::default()
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
        Part::Quote => Style::default()
            .fg(theme.gray)
            .add_modifier(Modifier::ITALIC),
        Part::Thread => Style::default()
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD),
        Part::Pin => Style::default().fg(theme.accent_verify),
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
    let replying_to = state
        .draft_parent()
        .and_then(|id| state.feed.iter().find(|m| m.id == id))
        .map(|parent| parent.from.clone());
    let title = match replying_to {
        _ if state.is_bot_dm() && state.is_bot_thinking => " hub-bot is thinking… ".to_string(),
        Some(author) if state.is_person_view() => format!(" reply to @{author} in {to} "),
        _ if state.is_person_view() && !state.composer.is_empty() => format!(" to {to} "),
        _ => String::new(),
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
            &if state.is_bot_dm() {
                "Ask @hub-bot about this hub, such as \"who works on login?\"".to_string()
            } else {
                format!("Message {to}")
            },
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
    if state.search.is_some() {
        return vec![
            HintItem::new(key!(Enter), "open"),
            HintItem::new(key!(Esc), "close search"),
            HintItem::paired(key!(Up), key!(Down), "choose"),
        ];
    }
    if state.show_pins {
        let mut hints = vec![HintItem::new(key!(Enter), "jump")];
        if state.is_person_view() {
            hints.push(HintItem::new(key!('u'), "unpin"));
        }
        hints.extend([
            HintItem::new(key!(Esc), "close pins"),
            HintItem::paired(key!(Up), key!(Down), "choose"),
        ]);
        return hints;
    }
    // Clickable hints come first, so a narrow footer keeps them.
    let mut hints = Vec::new();
    if state.is_person_view() {
        hints.push(HintItem::new(key!(Tab), "write"));
    }
    if state.thread.is_none() && state.is_person_view() && !state.feed.is_empty() {
        hints.push(HintItem::new(key!('r'), "reply"));
    }
    if state.selected_agent_peer().is_some() {
        hints.push(HintItem::new(key!('g'), "go to session"));
        hints.push(HintItem::new(key!('x'), "stop"));
    }
    let esc_label = if state.thread.is_some() {
        "back"
    } else {
        "close hub"
    };
    hints.extend([
        HintItem::new(key!('/'), "search"),
        HintItem::new(key!('p'), "perspective"),
        HintItem::new(key!('a'), "all/active DMs"),
        HintItem::new(key!(Esc), esc_label),
        HintItem::new(key!('?'), "keys"),
    ]);
    if matches!(state.selected_row(), Some(Row::Channel(_))) {
        hints.push(HintItem::new(key!('P'), "pins"));
    }
    hints.extend([
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
        Some(_) => member_summary(&state.snapshot, handle, &Theme::current()),
        None => String::new(),
    };
    (format!("@{handle}"), detail)
}

/// How the browser shows what a member does.
struct MemberState {
    glyph: &'static str,
    color: Color,
    /// Such as `working 3m`, `idle 12m`, `needs you`, or `gone`.
    word: String,
    task: Option<String>,
}

/// Purpose: Describe a member for the sidebar, the header, and tooltips.
/// Input: The snapshot, the handle, the time now, and the theme.
/// Output: The status glyph, its color, a short status word, and the task.
/// Note: A member that is not live is `gone`, whatever its last presence says.
fn member_state(snapshot: &HubSnapshot, handle: &str, now_ms: u64, theme: &Theme) -> MemberState {
    if handle == store::BOT_HANDLE {
        return MemberState {
            glyph: "✦ ",
            color: theme.accent_plan,
            word: "helper".to_string(),
            task: None,
        };
    }
    let Some(member) = snapshot.meta.member(handle) else {
        return MemberState {
            glyph: "○ ",
            color: theme.gray,
            word: "left".to_string(),
            task: None,
        };
    };
    if member.kind == MemberKind::Person {
        return MemberState {
            glyph: "◆ ",
            color: theme.accent_plan,
            word: "person".to_string(),
            task: None,
        };
    }
    let presence = snapshot.presences.get(handle);
    let task = presence.and_then(|p| p.task.clone());
    if !member.active {
        return MemberState {
            glyph: "○ ",
            color: theme.gray,
            word: "gone".to_string(),
            task,
        };
    }
    let Some(presence) = presence else {
        return MemberState {
            glyph: "● ",
            color: theme.accent_success,
            word: "live".to_string(),
            task,
        };
    };
    let age = age_label(now_ms.saturating_sub(presence.since_ms));
    let (glyph, color, word) = match presence.activity {
        store::Activity::Working => ("● ", theme.running, format!("working {age}")),
        store::Activity::Idle => ("● ", theme.accent_success, format!("idle {age}")),
        store::Activity::Waiting => ("◌ ", theme.accent_verify, "waiting".to_string()),
        store::Activity::NeedsInput => ("● ", theme.accent_error, "needs you".to_string()),
    };
    MemberState {
        glyph,
        color,
        word,
        task,
    }
}

/// One line about a member: its label, status, and task.
fn member_summary(snapshot: &HubSnapshot, handle: &str, theme: &Theme) -> String {
    let member_state = member_state(snapshot, handle, store::now_ms(), theme);
    let mut summary = match snapshot.meta.member(handle) {
        Some(member) if member.kind == MemberKind::Agent => {
            format!("{} · {}", member.label, member_state.word)
        }
        Some(_) => member_state.word,
        None if handle == store::BOT_HANDLE => {
            "read-only helper · ask what is going on and who works on what".to_string()
        }
        None => "left the hub".to_string(),
    };
    if let Some(task) = member_state.task {
        summary.push_str(&format!(" · {task}"));
    }
    summary
}

/// A short age, such as `now`, `40s`, `3m`, `2h`, or `5d`.
fn age_label(age_ms: u64) -> String {
    let secs = age_ms / 1000;
    match secs {
        0..=4 => "now".to_string(),
        5..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Purpose: Draw the member action buttons at the right of a DM header.
/// Input: The buffer, the header area, the state, and the theme.
/// Output: The x of the first button, or the area right edge with no buttons.
fn render_member_buttons(
    buf: &mut Buffer,
    area: Rect,
    state: &mut HubBrowserState,
    theme: &Theme,
) -> u16 {
    if state.selected_agent_peer().is_none() {
        return area.right();
    }
    let y = area.y + 1;
    let stop_label = "[stop]";
    let go_label = "[go to session]";
    let stop_x = area.right().saturating_sub(stop_label.width() as u16);
    let go_x = stop_x.saturating_sub(go_label.width() as u16 + 1);
    if go_x <= area.x + 10 {
        return area.right();
    }
    for (x, label, hover, is_stop) in [
        (go_x, go_label, HoverKey::HeaderGo, false),
        (stop_x, stop_label, HoverKey::HeaderStop, true),
    ] {
        let mut style = Style::default().fg(if is_stop {
            theme.accent_error
        } else {
            theme.command
        });
        if state.hover == Some(hover) {
            style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        }
        span_at(buf, x, y, label, style, area.right());
    }
    state.hits.header_go = Rect::new(go_x, y, go_label.width() as u16, 1);
    state.hits.header_stop = Rect::new(stop_x, y, stop_label.width() as u16, 1);
    go_x.saturating_sub(1)
}

/// Purpose: Draw a reply button at the right end of the hovered feed line.
/// Input: The buffer, the feed area, the screen row of the first feed line,
///     and the state.
/// Output: None. The button becomes a mouse target. Only the person can
///     reply, and a thread view needs no button.
fn render_reply_chip(
    buf: &mut Buffer,
    area: Rect,
    top: u16,
    state: &mut HubBrowserState,
    theme: &Theme,
) {
    let Some((px, py)) = state.pointer else {
        return;
    };
    if !state.is_person_view() || state.thread.is_some() || !contains(area, px, py) || py < top {
        return;
    }
    let line = (py - top) as usize;
    let Some(idx) = state.hits.feed_line_messages.get(line).copied().flatten() else {
        return;
    };
    let label = " ↩ reply ";
    let width = label.width() as u16;
    let rect = Rect::new(area.right().saturating_sub(width), py, width, 1);
    let mut style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_highlight);
    if contains(rect, px, py) {
        style = style.bg(theme.bg_hover).add_modifier(Modifier::UNDERLINED);
    }
    span_at(buf, rect.x, py, label, style, rect.right());
    let id = state.feed[idx].id.clone();
    state.hits.reply_chip = Some((rect, id.clone()));
    if !matches!(state.selected_row(), Some(Row::Channel(_))) {
        return;
    }
    let pin_label = if state.pinned.iter().any(|m| m.id == id) {
        " 📌 unpin "
    } else {
        " 📌 pin "
    };
    let pin_width = pin_label.width() as u16;
    let pin_x = rect.x.saturating_sub(pin_width + 1);
    if pin_x < area.x {
        return;
    }
    let pin_rect = Rect::new(pin_x, py, pin_width, 1);
    let mut style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_highlight);
    if contains(pin_rect, px, py) {
        style = style.bg(theme.bg_hover).add_modifier(Modifier::UNDERLINED);
    }
    span_at(buf, pin_x, py, pin_label, style, pin_rect.right());
    state.hits.pin_chip = Some((pin_rect, id));
}

/// Purpose: Draw the pinned messages of the channel over the top of the feed.
/// Input: The buffer, the feed area, the state, and the theme.
/// Output: None. Each pin row and each `[unpin]` button becomes a mouse
///     target. The person view alone gets the buttons.
fn render_pins_drawer(buf: &mut Buffer, feed: Rect, state: &mut HubBrowserState, theme: &Theme) {
    let Some(Row::Channel(name)) = state.selected_row() else {
        return;
    };
    let content_rows = state.pinned.len().max(1) as u16;
    let height = (content_rows + 2).min(feed.height);
    if height < 3 || feed.width < 20 {
        return;
    }
    let rect = Rect { height, ..feed };
    Clear.render(rect, buf);
    fill(buf, rect, Style::default().bg(theme.bg_base));
    let border = Style::default()
        .fg(theme.prompt_border_active)
        .bg(theme.bg_base);
    let is_person = state.is_person_view();
    let keys = if is_person {
        "Enter jump · u unpin · Esc close"
    } else {
        "Enter jump · Esc close"
    };
    let title = format!(" 📌 Pinned in #{name} · {keys} ");
    let inner_w = rect.width.saturating_sub(2) as usize;
    let top_fill = inner_w.saturating_sub(title.width() + 1);
    span_at(
        buf,
        rect.x,
        rect.y,
        &format!("╭─{title}{}╮", "─".repeat(top_fill)),
        border,
        rect.right(),
    );
    for y in rect.y + 1..rect.bottom() - 1 {
        span_at(buf, rect.x, y, "│", border, rect.right());
        span_at(buf, rect.right() - 1, y, "│", border, rect.right());
    }
    span_at(
        buf,
        rect.x,
        rect.bottom() - 1,
        &format!("╰{}╯", "─".repeat(inner_w)),
        border,
        rect.right(),
    );
    state.hits.pin_drawer = rect;
    let inner_x = rect.x + 2;
    let inner_right = rect.right() - 2;
    if state.pinned.is_empty() {
        let text = "No pinned messages. Hover a message and click 📌 pin.";
        span_at(buf, inner_x, rect.y + 1, text, theme.muted(), inner_right);
        return;
    }
    let visible = (height - 2) as usize;
    let first = (state.pins_selected + 1).saturating_sub(visible);
    let unpin_label = "[unpin]";
    let unpin_w = unpin_label.width() as u16;
    let mut rows = Vec::new();
    let mut unpins = Vec::new();
    for (offset, message) in state.pinned.iter().enumerate().skip(first).take(visible) {
        let y = rect.y + 1 + (offset - first) as u16;
        let row_rect = Rect::new(rect.x + 1, y, rect.width.saturating_sub(2), 1);
        let is_hovered = state.hover == Some(HoverKey::PinRow(y));
        let bg = if offset == state.pins_selected {
            theme.bg_highlight
        } else if is_hovered {
            theme.bg_hover
        } else {
            theme.bg_base
        };
        fill(buf, row_rect, Style::default().bg(bg));
        let text_right = if is_person {
            inner_right.saturating_sub(unpin_w + 1)
        } else {
            inner_right
        };
        let x = span_at(
            buf,
            inner_x,
            y,
            &format!("@{}", message.from),
            Style::default()
                .fg(theme.text_primary)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
            text_right,
        );
        let snippet = message.text.lines().next().unwrap_or_default();
        span_at(
            buf,
            x,
            y,
            &format!(" · {snippet}"),
            Style::default().fg(theme.gray_bright).bg(bg),
            text_right,
        );
        if is_person {
            let unpin_x = inner_right.saturating_sub(unpin_w);
            let unpin = Style::default().fg(theme.command).bg(bg);
            span_at(buf, unpin_x, y, unpin_label, unpin, inner_right);
            unpins.push((Rect::new(unpin_x, y, unpin_w, 1), message.id.clone()));
        }
        rows.push((y, message.id.clone()));
    }
    state.hits.pin_rows = rows;
    state.hits.pin_unpins = unpins;
}

/// Purpose: Draw a one-line card about the member under the pointer.
/// Input: The buffer, the feed area, and the state.
/// Output: None. The card sits under the hovered span, or above it on the
///     last feed line.
fn render_member_tooltip(buf: &mut Buffer, area: Rect, state: &HubBrowserState, theme: &Theme) {
    let Some((px, py)) = state.pointer else {
        return;
    };
    let Some(span) = state
        .hits
        .feed_spans
        .iter()
        .find(|span| span.row == py && span.cols.contains(&px))
    else {
        return;
    };
    let SpanTarget::Member(handle) = &span.target else {
        return;
    };
    let text = format!(
        " @{handle} · {} ",
        member_summary(&state.snapshot, handle, theme)
    );
    let y = if span.row + 1 < area.bottom() {
        span.row + 1
    } else {
        span.row.saturating_sub(1)
    };
    let width = (text.width() as u16).min(area.width);
    let x = span.cols.start.min(area.right().saturating_sub(width));
    let style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_highlight);
    span_at(buf, x, y, &text, style, x + width);
}

/// Purpose: Draw the hub search in the main pane: the query, then the results.
/// Input: The buffer, the main pane area without margins, and the state.
/// Output: None. Each result row becomes a mouse target.
fn render_search(buf: &mut Buffer, area: Rect, state: &mut HubBrowserState, theme: &Theme) {
    let Some(search) = state.search.as_ref() else {
        return;
    };
    let y = area.y + 1;
    let x = span_at(
        buf,
        area.x,
        y,
        "Search  ",
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
        area.right(),
    );
    let query_end = span_at(
        buf,
        x,
        y,
        &search.query,
        Style::default().fg(theme.text_primary),
        area.right(),
    );
    state.cursor = Some((query_end.min(area.right().saturating_sub(1)), y));
    span_at(
        buf,
        area.x,
        y + 1,
        &"─".repeat(area.width as usize),
        Style::default().fg(theme.gray_dim),
        area.right(),
    );
    let top = y + 2;
    let message = if search.query.trim().is_empty() {
        Some("Type to search every channel and DM of the hub.")
    } else if search.results.is_empty() {
        Some("No messages match.")
    } else {
        None
    };
    if let Some(message) = message {
        span_at(buf, area.x, top, message, theme.muted(), area.right());
        return;
    }
    let visible = area.bottom().saturating_sub(top) as usize;
    let first = (search.selected + 1).saturating_sub(visible);
    let mut rows = Vec::new();
    for (offset, result) in search.results.iter().enumerate().skip(first).take(visible) {
        let row_y = top + (offset - first) as u16;
        let row_rect = Rect::new(area.x, row_y, area.width, 1);
        let is_selected = offset == search.selected;
        let is_hovered = state
            .pointer
            .is_some_and(|(px, py)| contains(row_rect, px, py));
        let bg = if is_selected {
            theme.bg_highlight
        } else if is_hovered {
            theme.bg_hover
        } else {
            theme.bg_base
        };
        fill(buf, row_rect, Style::default().bg(bg));
        let place = if store::split_dm_conv(&result.conv).is_some() {
            "DM".to_string()
        } else {
            result.conv.clone()
        };
        let time = local_time(result.ts_unix_ms).format("%b %-d %-I:%M %p");
        let head = format!("{place} · @{} · {time}  ", result.from);
        let x = span_at(
            buf,
            area.x,
            row_y,
            &head,
            theme.muted().bg(bg),
            area.right(),
        );
        let text = result.text.lines().next().unwrap_or_default();
        span_at(
            buf,
            x,
            row_y,
            text,
            Style::default().fg(theme.text_primary).bg(bg),
            area.right(),
        );
        rows.push((row_y, result.id.clone()));
    }
    state.hits.search_rows = rows;
}

/// Purpose: Draw the key help over the browser.
/// Input: The buffer, the browser area, and the theme.
/// Output: None. Any key or click closes it.
fn render_help(buf: &mut Buffer, area: Rect, theme: &Theme) {
    const KEYS: [(&str, &str); 19] = [
        ("↑ ↓ / j k", "choose a conversation"),
        ("] [", "next / previous unread conversation"),
        ("Tab / i / Enter", "write a post"),
        ("Shift+Enter", "new line in a post"),
        ("@ #", "complete a member or a channel"),
        ("r", "reply to the newest message"),
        ("/", "search the whole hub"),
        ("@hub-bot DM", "ask a helper about the hub"),
        ("g", "go to the session of the DM agent"),
        ("x x", "stop the DM agent"),
        ("p", "view the hub as another member"),
        ("a", "all or active DMs"),
        ("P", "pinned messages of the channel"),
        ("click 📌 pin", "pin or unpin a channel message"),
        ("PgUp PgDn End", "scroll the feed"),
        ("click @name", "open the DM with that member"),
        ("click ↩ / quote", "reply / open the thread"),
        ("drag", "copy text"),
        ("Esc", "back, then close the hub"),
    ];
    let width = 64.min(area.width);
    let height = (KEYS.len() as u16 + 4).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    Clear.render(rect, buf);
    fill(buf, rect, Style::default().bg(theme.bg_base));
    let border = Style::default()
        .fg(theme.prompt_border_active)
        .bg(theme.bg_base);
    let inner_w = rect.width.saturating_sub(2) as usize;
    span_at(
        buf,
        rect.x,
        rect.y,
        &format!("╭{}╮", "─".repeat(inner_w)),
        border,
        rect.right(),
    );
    for y in rect.y + 1..rect.bottom() - 1 {
        span_at(buf, rect.x, y, "│", border, rect.right());
        span_at(buf, rect.right() - 1, y, "│", border, rect.right());
    }
    span_at(
        buf,
        rect.x,
        rect.bottom() - 1,
        &format!("╰{}╯", "─".repeat(inner_w)),
        border,
        rect.right(),
    );
    let inner_x = rect.x + 2;
    let inner_right = rect.right() - 2;
    span_at(
        buf,
        inner_x,
        rect.y + 1,
        "Hub keys · any key closes",
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
        inner_right,
    );
    for (idx, (key, label)) in KEYS.iter().enumerate() {
        let y = rect.y + 2 + idx as u16;
        if y >= rect.bottom() - 1 {
            break;
        }
        let x = span_at(
            buf,
            inner_x,
            y,
            &format!("{key:<18}"),
            Style::default()
                .fg(theme.gray_bright)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD),
            inner_right,
        );
        span_at(
            buf,
            x,
            y,
            label,
            theme.muted().bg(theme.bg_base),
            inner_right,
        );
    }
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
                dm(store::BOT_HANDLE, "me"),
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
        assert_eq!(state.post_target().as_deref(), Some("@hub-bot"));
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

    fn texts(state: &HubBrowserState) -> Vec<&str> {
        state.feed.iter().map(|m| m.text.as_str()).collect()
    }

    fn post(hub: &store::Hub, from: &str, conv: &str, text: &str) {
        hub.append(&message(from, conv.to_string(), text, store::now_ms()))
            .unwrap();
    }

    /// Run one background refresh the way the event loop does.
    fn refresh(state: &mut HubBrowserState) -> Option<Vec<ReadMark>> {
        let job = state.begin_refresh().expect("no refresh in flight");
        let generation = job.generation;
        let result = job.run().map_err(|error| error.to_string());
        state.finish_refresh(generation, result)
    }

    #[test]
    fn a_background_refresh_shows_new_messages_and_returns_the_read_position() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub.clone(), "me", Some("#general")).unwrap();
        post(&hub, "front", "#general", "news");
        post(&hub, "back", &store::dm_conv("me", "back"), "ping");

        let job = state.begin_refresh().unwrap();
        assert!(state.begin_refresh().is_none(), "one refresh at a time");
        let generation = job.generation;
        let marks = state
            .finish_refresh(generation, job.run().map_err(|e| e.to_string()))
            .expect("the result is current");
        assert_eq!(texts(&state), vec!["hello", "news"]);
        assert_eq!(state.snapshot.unread(&store::dm_conv("me", "back")), 1);
        let len = std::fs::metadata(hub.dir().join("channels/general.jsonl"))
            .unwrap()
            .len();
        let offset = hub
            .messages_from_offset("#general", 0, None, None)
            .unwrap()
            .next_byte_offset;
        assert_eq!(offset.map(|offset| offset.bytes), Some(len));
        let mark = ReadMark {
            conv: "#general".to_string(),
            count: 2,
            offset,
        };
        assert_eq!(marks, vec![mark]);
        // The apply wrote no cursor: the caller saves the marks off-thread.
        assert_eq!(hub.cursors("me").unwrap().read.get("#general"), Some(&1));
        assert!(state.begin_refresh().is_some(), "the flag clears on apply");
    }

    #[test]
    fn a_refresh_result_is_dropped_after_a_local_change() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub.clone(), "me", Some("#general")).unwrap();
        post(&hub, "front", "#general", "news");
        let job = state.begin_refresh().unwrap();
        let generation = job.generation;
        let result = job.run().map_err(|e| e.to_string());
        // The person picks another conversation while the job runs.
        state.select(Some(dm("front", "me")));
        let feed_before = state.feed.clone();
        assert_eq!(state.finish_refresh(generation, result), None);
        assert_eq!(state.feed, feed_before);
        assert!(state.begin_refresh().is_some(), "the flag still clears");

        // A browser that opened later never takes the result of an older one.
        let mut old = HubBrowserState::open(hub.clone(), "me", None).unwrap();
        let job = old.begin_refresh().unwrap();
        let generation = job.generation;
        let mut reopened = HubBrowserState::open(hub, "me", None).unwrap();
        let result = job.run().map_err(|e| e.to_string());
        assert_eq!(reopened.finish_refresh(generation, result), None);
    }

    #[test]
    fn incremental_reloads_match_a_full_reload_after_appends_and_truncation() {
        let (_root, hub) = hub_with_dms();
        let mut state = HubBrowserState::open(hub.clone(), "me", Some("#general")).unwrap();
        let fresh = |hub: &store::Hub| {
            let fresh = HubBrowserState::open(hub.clone(), "me", Some("#general")).unwrap();
            (fresh.feed.clone(), fresh.snapshot.unread_counts.clone())
        };
        for round in 0..3 {
            post(&hub, "front", "#general", &format!("round {round}"));
            post(
                &hub,
                "back",
                &store::dm_conv("me", "back"),
                &format!("dm {round}"),
            );
            refresh(&mut state).unwrap();
            let (feed, unread) = fresh(&hub);
            assert_eq!(state.feed, feed);
            assert_eq!(state.snapshot.unread_counts, unread);
        }
        assert_eq!(state.snapshot.unread(&store::dm_conv("me", "back")), 3);

        // Another writer replaces the channel file with a shorter one.
        let path = hub.dir().join("channels/general.jsonl");
        let line = serde_json::to_string(&message(
            "front",
            "#general".to_string(),
            "only line",
            store::now_ms(),
        ))
        .unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();
        state.reload().unwrap();
        assert_eq!(texts(&state), vec!["only line"]);
        assert_eq!(state.feed, fresh(&hub).0);
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
                            pins: Vec::new(),
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
        state.select(Some(dm("front", "me")));
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
        assert!(matches!(lines[5][0].1, Part::Person(_)));
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
                pins: Vec::new(),
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

    fn save_presence(hub: &store::Hub, handle: &str, activity: store::Activity, task: &str) {
        let presence = store::Presence {
            activity,
            since_ms: store::now_ms().saturating_sub(180_000),
            task: Some(task.to_string()),
        };
        hub.save_presence(handle, &presence).unwrap();
    }

    #[test]
    fn the_sidebar_and_header_show_what_each_agent_does() {
        let (_root, hub) = hub_with_dms();
        save_presence(
            &hub,
            "front",
            store::Activity::Working,
            "build the login page",
        );
        save_presence(&hub, "back", store::Activity::NeedsInput, "migrate the db");
        let mut state = open(hub);
        state.select(Some(dm("front", "me")));
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let text = buffer_text(&buf);
        assert!(text.contains("working 3m"), "{text}");
        assert!(text.contains("needs you"), "{text}");
        assert!(text.contains("build the login page"), "{text}");
        assert!(text.contains("[go to session]"), "{text}");
        assert!(text.contains("go to session"), "footer hint: {text}");
    }

    #[test]
    fn stop_needs_two_presses_and_go_names_the_member() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.select(Some(dm("front", "me")));
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('g'))),
            HubKeyOutcome::GoToMember("front".to_string())
        );
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('x'))),
            HubKeyOutcome::Changed
        );
        assert!(state.notice.as_deref().is_some_and(|n| n.contains("again")));
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('x'))),
            HubKeyOutcome::StopMember("front".to_string())
        );
        state.handle_key(&key(KeyCode::Char('x')));
        state.handle_key(&key(KeyCode::Down));
        assert_ne!(
            state.handle_key(&key(KeyCode::Char('x'))),
            HubKeyOutcome::StopMember("front".to_string()),
            "another key in between resets the confirmation"
        );
    }

    #[test]
    fn member_actions_ignore_persons_and_channels() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('g'))),
            HubKeyOutcome::Changed
        );
        state.select(Some(dm("me", "me")));
        assert_eq!(state.selected_agent_peer(), None);
    }

    #[test]
    fn hovering_a_handle_shows_a_member_card() {
        let (root, hub) = hub_with_dms();
        save_presence(&hub, "front", store::Activity::Idle, "write the docs");
        hub.append(&message(
            "front",
            "#general".to_string(),
            "ready",
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, &mut state);
        let (x, y) = span_cell(&state, &SpanTarget::Member("front".to_string()));
        state.handle_mouse(MouseEventKind::Moved, x, y);
        render_hub_browser(&mut buf, area, &mut state);
        let text = buffer_text(&buf);
        assert!(
            text.contains("@front · main · idle 3m · write the docs"),
            "{text}"
        );
        drop(root);
    }

    #[test]
    fn age_labels_are_short() {
        assert_eq!(age_label(1_000), "now");
        assert_eq!(age_label(42_000), "42s");
        assert_eq!(age_label(5 * 60_000), "5m");
        assert_eq!(age_label(3 * 3_600_000), "3h");
        assert_eq!(age_label(2 * 86_400_000), "2d");
    }

    fn render(state: &mut HubBrowserState) -> Buffer {
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_hub_browser(&mut buf, area, state);
        buf
    }

    /// A #general post from @front with one reply from @back.
    fn hub_with_thread() -> (tempfile::TempDir, store::Hub, String) {
        let (root, hub) = hub_with_dms();
        let parent = message("front", "#general".to_string(), "plan?", store::now_ms());
        let mut reply = message(
            "back",
            "#general".to_string(),
            "use sqlite",
            store::now_ms(),
        );
        reply.reply_to = Some(parent.id.clone());
        hub.append(&parent).unwrap();
        hub.append(&reply).unwrap();
        (root, hub, parent.id)
    }

    #[test]
    fn a_click_on_the_reply_count_opens_the_thread_and_esc_closes_it() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("↩ 1 reply"), "{text}");
        assert!(text.contains("↳ @front: plan?"), "quote line: {text}");
        let cell = span_cell(&state, &SpanTarget::Thread(parent_id.clone()));
        click(&mut state, cell);
        assert_eq!(state.thread.as_deref(), Some(parent_id.as_str()));
        let ids: Vec<&str> = state.feed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "the parent and its reply only: {ids:?}");
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("Thread"), "{text}");
        assert!(text.contains("[← back]"), "{text}");
        assert!(text.contains("reply to @front in #general"), "{text}");
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.thread, None);
        assert!(state.feed.len() > 2);
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
    }

    #[test]
    fn a_post_in_a_thread_is_a_reply_to_its_root() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        state.open_thread(&parent_id);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("agreed");
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Reply {
                to: "#general".to_string(),
                text: "agreed".to_string(),
                reply_to: parent_id,
            }
        );
    }

    /// A #general root with a legacy chain on disk: m2 replies to m1, and m3
    /// replies to m2.
    fn hub_with_nested_replies() -> (tempfile::TempDir, store::Hub, [String; 3]) {
        let (root, hub) = hub_with_dms();
        let general = || "#general".to_string();
        let m1 = message("front", general(), "plan?", store::now_ms());
        let mut m2 = message("back", general(), "use sqlite", store::now_ms());
        m2.reply_to = Some(m1.id.clone());
        let mut m3 = message("front", general(), "why sqlite?", store::now_ms());
        m3.reply_to = Some(m2.id.clone());
        for message in [&m1, &m2, &m3] {
            hub.append(message).unwrap();
        }
        (root, hub, [m1.id, m2.id, m3.id])
    }

    #[test]
    fn a_legacy_reply_to_a_reply_shows_in_the_root_thread() {
        let (_root, hub, [m1, m2, m3]) = hub_with_nested_replies();
        let mut state = open(hub);
        let counts = reply_counts(&state.feed);
        assert_eq!(counts.get(m1.as_str()), Some(&2));
        assert_eq!(counts.get(m2.as_str()), None);
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("↩ 2 replies"), "{text}");
        for id in [&m1, &m2, &m3] {
            state.open_thread(id);
            assert_eq!(state.thread.as_deref(), Some(m1.as_str()));
            let ids: Vec<&str> = state.feed.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, [m1.as_str(), m2.as_str(), m3.as_str()]);
            state.handle_key(&key(KeyCode::Esc));
            assert_eq!(state.thread, None);
        }
    }

    #[test]
    fn a_click_on_the_quote_of_a_nested_reply_opens_the_root_thread() {
        let (_root, hub, [m1, m2, _]) = hub_with_nested_replies();
        let mut state = open(hub);
        render(&mut state);
        // Only the quote above m3 points at m2.
        let cell = span_cell(&state, &SpanTarget::Thread(m2));
        click(&mut state, cell);
        assert_eq!(state.thread.as_deref(), Some(m1.as_str()));
        assert_eq!(state.feed.len(), 3);
    }

    #[test]
    fn a_reply_to_a_reply_targets_the_thread_root() {
        let (_root, hub, [m1, m2, _]) = hub_with_nested_replies();
        let mut state = open(hub);
        state.start_reply(m2);
        assert_eq!(state.reply_to.as_deref(), Some(m1.as_str()));
        state.handle_paste("agreed");
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Reply {
                to: "#general".to_string(),
                text: "agreed".to_string(),
                reply_to: m1,
            }
        );
    }

    #[test]
    fn r_replies_to_the_newest_message_and_esc_drops_the_reply() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('r')));
        assert!(state.is_composing);
        // The newest message is a reply, so the draft replies to its root.
        assert_eq!(state.reply_to.as_deref(), Some(parent_id.as_str()));
        state.handle_key(&key(KeyCode::Esc));
        assert!(!state.is_composing);
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.reply_to, None);
        assert_ne!(
            state.handle_key(&key(KeyCode::Esc)),
            HubKeyOutcome::Changed,
            "the third Esc closes the hub"
        );
    }

    #[test]
    fn a_click_on_the_reply_chip_starts_a_reply() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        render(&mut state);
        let (x, y) = span_cell(&state, &SpanTarget::Member("front".to_string()));
        state.handle_mouse(MouseEventKind::Moved, x, y);
        render(&mut state);
        let (rect, id) = state.hits.reply_chip.clone().expect("chip on hover");
        assert_eq!(id, parent_id);
        click(&mut state, (rect.x + 1, rect.y));
        assert!(state.is_composing);
        assert_eq!(state.reply_to.as_deref(), Some(parent_id.as_str()));
        assert!(state.feed_selection.is_none(), "no text selection starts");
    }

    #[test]
    fn r_keeps_a_paused_draft_for_another_conversation() {
        let (_root, hub, _) = hub_with_thread();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("draft");
        state.handle_key(&key(KeyCode::Esc));
        state.select(Some(dm("front", "me")));
        state.handle_key(&key(KeyCode::Char('r')));
        assert_eq!(state.reply_to, None);
        assert_eq!(state.post_target().as_deref(), Some("#general"));
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("#general")),
            "{:?}",
            state.notice
        );
    }

    #[test]
    fn a_thread_view_does_not_mark_its_conversation_read() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub.clone());
        state.open_thread(&parent_id);
        let read_before = hub.cursors("me").unwrap().read.get("#general").copied();
        hub.append(&message(
            "front",
            "#general".to_string(),
            "unrelated",
            store::now_ms(),
        ))
        .unwrap();
        state.reload().unwrap();
        assert_eq!(state.feed.len(), 2, "the thread only");
        let read_after = hub.cursors("me").unwrap().read.get("#general").copied();
        assert_eq!(read_after, read_before);
        assert_eq!(state.snapshot.unread("#general"), 1);
    }

    #[test]
    fn a_search_hit_older_than_the_feed_window_is_loaded() {
        let (_root, hub) = hub_with_dms();
        let old = message("front", "#general".to_string(), "needle", store::now_ms());
        hub.append(&old).unwrap();
        for i in 0..FEED_LIMIT + 10 {
            let text = format!("filler {i}");
            hub.append(&message(
                "back",
                "#general".to_string(),
                &text,
                store::now_ms(),
            ))
            .unwrap();
        }
        let mut state = open(hub);
        assert!(state.feed.iter().all(|m| m.id != old.id));
        state.handle_key(&key(KeyCode::Char('/')));
        for c in "needle".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        state.handle_key(&key(KeyCode::Enter));
        assert_eq!(state.focus_message.as_deref(), Some(old.id.as_str()));
        assert!(state.feed.iter().any(|m| m.id == old.id));
        assert_eq!(
            state.feed.last().map(|m| m.text.as_str()),
            Some("filler 309")
        );
    }

    #[test]
    fn a_search_hit_in_the_same_conversation_leaves_the_thread() {
        let (_root, hub, parent_id) = hub_with_thread();
        hub.append(&message(
            "front",
            "#general".to_string(),
            "unrelated note",
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        state.open_thread(&parent_id);
        state.handle_key(&key(KeyCode::Char('/')));
        for c in "unrelated".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        state.handle_key(&key(KeyCode::Enter));
        assert_eq!(state.thread, None);
        assert!(state.feed.iter().any(|m| m.text == "unrelated note"));
    }

    #[test]
    fn a_paused_draft_keeps_its_reply_parent_in_another_conversation() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('r')));
        state.handle_paste("agreed");
        state.handle_key(&key(KeyCode::Esc));
        state.select(Some(dm("front", "me")));
        state.handle_key(&key(KeyCode::Tab));
        // The newest message is a reply, so the draft replies to its root.
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Reply {
                to: "#general".to_string(),
                text: "agreed".to_string(),
                reply_to: parent_id,
            }
        );
    }

    #[test]
    fn a_paused_draft_sent_from_another_thread_takes_no_parent_from_it() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        state.select(Some(dm("front", "me")));
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("dm draft");
        state.handle_key(&key(KeyCode::Esc));
        state.select(Some(Row::Channel("general".to_string())));
        state.open_thread(&parent_id);
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "@front".to_string(),
                text: "dm draft".to_string(),
            }
        );
    }

    #[test]
    fn a_quote_of_another_conversation_opens_no_thread() {
        let (_root, hub) = hub_with_dms();
        let dm_message = message(
            "front",
            store::dm_conv("front", "me"),
            "in a DM",
            store::now_ms(),
        );
        hub.append(&dm_message).unwrap();
        let mut reply = message("back", "#general".to_string(), "cross", store::now_ms());
        reply.reply_to = Some(dm_message.id.clone());
        hub.append(&reply).unwrap();
        let mut state = open(hub);
        state.open_thread(&dm_message.id);
        assert_eq!(state.thread, None);
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("another conversation"))
        );
    }

    #[test]
    fn the_open_picker_gets_clicks_over_search_results() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('/')));
        for c in "e".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        render(&mut state);
        state.open_picker();
        render(&mut state);
        let search_ys: Vec<u16> = state.hits.search_rows.iter().map(|(y, _)| *y).collect();
        let (rect, handle) = state
            .hits
            .picker
            .iter()
            .find(|(rect, _)| search_ys.contains(&rect.y))
            .cloned()
            .expect("a picker row over a search row");
        click(&mut state, (rect.x + 1, rect.y));
        assert_eq!(state.perspective, handle);
        assert!(state.picker.is_none());
    }

    #[test]
    fn a_paused_thread_draft_stays_a_reply_after_the_thread_closes() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub);
        state.open_thread(&parent_id);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("late reply");
        state.handle_key(&key(KeyCode::Esc));
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.thread, None);
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Reply {
                to: "#general".to_string(),
                text: "late reply".to_string(),
                reply_to: parent_id,
            }
        );
    }

    #[test]
    fn a_thread_longer_than_the_window_keeps_its_root() {
        let (_root, hub, parent_id) = hub_with_thread();
        for i in 0..FEED_LIMIT + 5 {
            let mut reply = message(
                "back",
                "#general".to_string(),
                &format!("r{i}"),
                store::now_ms(),
            );
            reply.reply_to = Some(parent_id.clone());
            hub.append(&reply).unwrap();
        }
        let mut state = open(hub);
        state.open_thread(&parent_id);
        assert_eq!(state.thread.as_deref(), Some(parent_id.as_str()));
        assert_eq!(state.feed[0].id, parent_id);
    }

    #[test]
    fn a_click_over_the_help_only_closes_it() {
        let (_root, mut state) = open_with_general_post("ready");
        let cell = span_cell(&state, &SpanTarget::Member("front".to_string()));
        state.handle_key(&key(KeyCode::Char('?')));
        render(&mut state);
        click(&mut state, cell);
        assert!(!state.show_help);
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string()))
        );
    }

    #[test]
    fn a_message_that_arrives_during_search_stays_unread_until_search_closes() {
        let (_root, hub, _) = hub_with_thread();
        let mut state = open(hub.clone());
        state.handle_key(&key(KeyCode::Char('/')));
        hub.append(&message(
            "front",
            "#general".to_string(),
            "new",
            store::now_ms(),
        ))
        .unwrap();
        state.reload().unwrap();
        assert_eq!(state.snapshot.unread("#general"), 1);
        state.handle_key(&key(KeyCode::Esc));
        assert_eq!(state.snapshot.unread("#general"), 0);
    }

    #[test]
    fn a_paste_goes_into_the_search_query() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('/')));
        state.handle_paste("status");
        let search = state.search.as_ref().unwrap();
        assert_eq!(search.query, "status");
        assert!(!search.results.is_empty());
    }

    #[test]
    fn a_search_hit_keeps_the_parent_of_a_paused_thread_draft() {
        let (_root, hub, parent_id) = hub_with_thread();
        hub.append(&message(
            "front",
            "#general".to_string(),
            "unrelated note",
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        state.open_thread(&parent_id);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("thread draft");
        state.handle_key(&key(KeyCode::Esc));
        state.handle_key(&key(KeyCode::Char('/')));
        state.handle_paste("unrelated");
        state.handle_key(&key(KeyCode::Enter));
        assert_eq!(state.thread, None);
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Reply {
                to: "#general".to_string(),
                text: "thread draft".to_string(),
                reply_to: parent_id,
            }
        );
    }

    #[test]
    fn the_unread_jump_leaves_a_thread_of_the_same_conversation() {
        let (_root, hub, parent_id) = hub_with_thread();
        let mut state = open(hub.clone());
        state.open_thread(&parent_id);
        hub.append(&message(
            "front",
            "#general".to_string(),
            "outside",
            store::now_ms(),
        ))
        .unwrap();
        state.reload().unwrap();
        state.handle_key(&key(KeyCode::Char(']')));
        assert_eq!(state.thread, None);
        assert!(state.feed.iter().any(|m| m.text == "outside"));
    }

    #[test]
    fn a_rejected_thread_does_not_give_its_root_to_a_paused_draft() {
        let (_root, hub) = hub_with_dms();
        let dm_message = message(
            "front",
            store::dm_conv("front", "me"),
            "in a DM",
            store::now_ms(),
        );
        hub.append(&dm_message).unwrap();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("plain draft");
        state.handle_key(&key(KeyCode::Esc));
        state.open_thread(&dm_message.id);
        assert_eq!(state.thread, None);
        assert_eq!(state.reply_to, None);
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "#general".to_string(),
                text: "plain draft".to_string(),
            }
        );
    }

    /// `hub_with_thread` plus a second #general root with one reply.
    fn hub_with_two_threads() -> (tempfile::TempDir, store::Hub, String, String) {
        let (root, hub, first) = hub_with_thread();
        let second = message("back", "#general".to_string(), "deploy?", store::now_ms());
        let mut reply = message("front", "#general".to_string(), "friday", store::now_ms());
        reply.reply_to = Some(second.id.clone());
        hub.append(&second).unwrap();
        hub.append(&reply).unwrap();
        (root, hub, first, second.id)
    }

    #[test]
    fn browsing_another_thread_keeps_the_parent_of_a_paused_draft() {
        let (_root, hub, first, second) = hub_with_two_threads();
        let mut state = open(hub);
        state.open_thread(&first);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("for the first");
        state.handle_key(&key(KeyCode::Esc));
        state.handle_key(&key(KeyCode::Esc));
        state.open_thread(&second);
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Reply {
                to: "#general".to_string(),
                text: "for the first".to_string(),
                reply_to: first,
            }
        );
    }

    #[test]
    fn browsing_a_thread_keeps_a_paused_post_top_level() {
        let (_root, hub, first, _) = hub_with_two_threads();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Tab));
        state.handle_paste("plain post");
        state.handle_key(&key(KeyCode::Esc));
        state.open_thread(&first);
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            state.handle_key(&key(KeyCode::Enter)),
            HubKeyOutcome::Post {
                to: "#general".to_string(),
                text: "plain post".to_string(),
            }
        );
    }

    #[test]
    fn search_finds_a_dm_and_enter_opens_it_at_the_message() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('/')));
        assert!(state.search.is_some());
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("Type to search"), "{text}");
        for c in "DONE".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("done: 5"), "{text}");
        assert_eq!(state.hits.search_rows.len(), 1);
        let found = state.search.as_ref().unwrap().results[0].id.clone();
        state.handle_key(&key(KeyCode::Enter));
        assert!(state.search.is_none());
        assert_eq!(state.perspective, "back");
        assert_eq!(state.selected_row(), Some(dm("front", "back")));
        assert_eq!(state.focus_message.as_deref(), Some(found.as_str()));
    }

    #[test]
    fn a_click_on_a_search_row_opens_it() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('/')));
        for c in "status".chars() {
            state.handle_key(&key(KeyCode::Char(c)));
        }
        render(&mut state);
        let (row_y, _) = state.hits.search_rows[0].clone();
        click(&mut state, (40, row_y));
        assert_eq!(state.selected_row(), Some(dm("front", "me")));
    }

    #[test]
    fn brackets_jump_to_the_next_unread_conversation() {
        let (_root, hub) = hub_with_dms();
        hub.append(&message(
            "back",
            store::dm_conv("back", "me"),
            "ping",
            store::now_ms(),
        ))
        .unwrap();
        let mut state = open(hub);
        state.select(Some(Row::Channel("general".to_string())));
        state.handle_key(&key(KeyCode::Char(']')));
        assert_eq!(state.selected_row(), Some(dm("back", "me")));
        state.handle_key(&key(KeyCode::Char(']')));
        assert_eq!(
            state.selected_row(),
            Some(dm("back", "me")),
            "no other unread"
        );
        assert!(state.notice.is_some());
    }

    #[test]
    fn question_mark_shows_the_keys_and_any_key_closes_them() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Char('?')));
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("Hub keys"), "{text}");
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('j'))),
            HubKeyOutcome::Changed
        );
        assert!(!state.show_help);
        assert_eq!(
            state.selected_row(),
            Some(Row::Channel("general".to_string())),
            "the closing key does nothing else"
        );
    }

    #[test]
    fn hub_bot_is_the_first_dm_of_the_person_only() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        let rows = state.rows();
        let dms = rows
            .iter()
            .position(|r| *r == Row::Section(DM_SECTION))
            .unwrap();
        assert_eq!(rows[dms + 1], dm(store::BOT_HANDLE, "me"));
        state.toggle_dm_filter().unwrap();
        assert!(state.rows().contains(&dm(store::BOT_HANDLE, "me")));
        state.set_perspective("front").unwrap();
        assert!(
            !state
                .rows()
                .iter()
                .any(|r| matches!(r, Row::Dm { peer, .. } if peer == store::BOT_HANDLE))
        );
    }

    #[test]
    fn a_click_on_at_hub_bot_opens_its_dm_with_an_ask_prompt() {
        let (_root, mut state) = open_with_general_post("ask @hub-bot");
        let cell = span_cell(&state, &SpanTarget::Member(store::BOT_HANDLE.to_string()));
        click(&mut state, cell);
        assert!(state.is_bot_dm());
        assert_eq!(state.post_target().as_deref(), Some("@hub-bot"));
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("Ask @hub-bot about this hub"), "{text}");
        assert!(text.contains("read-only helper"), "{text}");
        state.is_bot_thinking = true;
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("hub-bot is thinking"), "{text}");
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

    #[test]
    fn a_clipped_sender_name_opens_the_full_handle() {
        let from = "backend-architect-2";
        let msg = message(from, "#general".into(), "hi", 0);
        let persons = BTreeSet::new();
        let mentions = BTreeSet::new();
        let names = FeedNames {
            persons: &persons,
            mentions: &mentions,
        };
        let today = local_time(0).date_naive();
        let lines = feed_lines(std::slice::from_ref(&msg), 18, None, &names, today);
        let (text, part) = lines
            .iter()
            .flatten()
            .find(|(_, part)| matches!(part, Part::Agent(..)))
            .expect("sender span");
        assert_ne!(text, &format!("@{from}"), "the name is clipped");
        let (_root, hub) = hub_with_dms();
        hub.update_meta(|meta| {
            meta.members
                .push(member("backend-architect", MemberKind::Agent));
            meta.members.push(member(from, MemberKind::Agent));
        })
        .unwrap();
        let meta = hub.meta().unwrap();
        assert_eq!(
            span_target(text, part, &meta),
            Some(SpanTarget::Member(from.to_string()))
        );
    }

    /// A hub whose #general has one old pinned message and more than
    /// FEED_LIMIT newer messages.
    fn hub_with_old_pin() -> (tempfile::TempDir, store::Hub, store::Message) {
        let (root, hub) = hub_with_dms();
        let old = message("front", "#general".to_string(), "decided: REST", 1);
        hub.append(&old).unwrap();
        for i in 0..FEED_LIMIT + 10 {
            let text = format!("filler {i}");
            hub.append(&message(
                "back",
                "#general".to_string(),
                &text,
                store::now_ms(),
            ))
            .unwrap();
        }
        agent_hub::pin_as_person(&hub, "me", "#general", &old.id).unwrap();
        (root, hub, old)
    }

    #[test]
    fn the_pin_pill_opens_a_drawer_that_jumps_to_an_old_pin() {
        let (_root, hub, old) = hub_with_old_pin();
        let mut state = open(hub);
        assert_eq!(state.pinned.len(), 1);
        assert!(
            state.feed.iter().all(|m| m.id != old.id),
            "outside the window"
        );
        let buf = render(&mut state);
        let pill = state.hits.pin_pill;
        assert!(pill.width > 0, "the header shows the pill");
        assert!(buffer_text(&buf).contains("1]"));

        click(&mut state, (pill.x + 1, pill.y));
        assert!(state.show_pins, "a click on the pill opens the drawer");
        let buf = render(&mut state);
        let text = buffer_text(&buf);
        assert!(text.contains("Pinned in #general"), "{text}");
        assert!(text.contains("@front · decided: REST"), "{text}");
        assert!(text.contains("[unpin]"), "{text}");

        state.handle_key(&key(KeyCode::Enter));
        assert!(!state.show_pins);
        assert_eq!(state.focus_message.as_deref(), Some(old.id.as_str()));
        assert!(
            state.feed.iter().any(|m| m.id == old.id),
            "the old pin loads"
        );
        let text = buffer_text(&render(&mut state));
        assert!(text.contains("decided: REST"), "{text}");
        assert!(text.contains("pinned"), "the feed marks the pin:\n{text}");

        state.handle_key(&key(KeyCode::Char('P')));
        assert!(state.show_pins);
        state.handle_key(&key(KeyCode::Down));
        assert!(state.show_pins, "a drawer key keeps the drawer");
        state.handle_key(&key(KeyCode::Char('/')));
        assert!(!state.show_pins, "another key closes the drawer");
        assert!(state.search.is_some(), "and then works as usual");
    }

    #[test]
    fn p_on_a_dm_opens_no_drawer() {
        let (_root, hub) = hub_with_dms();
        let mut state = open(hub);
        state.handle_key(&key(KeyCode::Down));
        state.handle_key(&key(KeyCode::Char('P')));
        assert!(!state.show_pins);
        assert!(state.notice.as_deref().unwrap().contains("Only channels"));
    }

    #[test]
    fn the_hover_pin_chip_asks_to_pin_or_unpin_in_the_person_view_only() {
        let (_root, mut state) = open_with_general_post("ready");
        let id = state.feed.last().unwrap().id.clone();
        render(&mut state);
        let (x, y) = span_cell(&state, &SpanTarget::Member("front".to_string()));
        state.handle_mouse(MouseEventKind::Moved, x, y);
        render(&mut state);
        let (rect, chip_id) = state.hits.pin_chip.clone().expect("pin chip on hover");
        assert_eq!(chip_id, id);
        let outcome = state.handle_mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
        assert_eq!(
            outcome,
            HubKeyOutcome::Pin {
                channel: "#general".to_string(),
                message_id: id.clone(),
            }
        );
        assert!(state.feed_selection.is_none(), "no text selection starts");

        agent_hub::pin_as_person(&state.hub, "me", "#general", &id).unwrap();
        state.reload().unwrap();
        render(&mut state);
        state.handle_mouse(MouseEventKind::Moved, x, y);
        render(&mut state);
        let (rect, _) = state.hits.pin_chip.clone().expect("unpin chip");
        let outcome = state.handle_mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
        assert!(
            matches!(outcome, HubKeyOutcome::Unpin { .. }),
            "{outcome:?}"
        );

        state.set_perspective("front").unwrap();
        render(&mut state);
        state.handle_mouse(MouseEventKind::Moved, x, y);
        render(&mut state);
        assert!(
            state.hits.pin_chip.is_none(),
            "a read-only view has no chip"
        );
        state.handle_key(&key(KeyCode::Char('P')));
        assert!(state.show_pins, "a read-only view still shows the pins");
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('u'))),
            HubKeyOutcome::Changed
        );
    }

    #[test]
    fn a_pinned_message_gets_its_own_header_with_a_badge() {
        let persons = BTreeSet::new();
        let mentions = BTreeSet::new();
        let names = FeedNames {
            persons: &persons,
            mentions: &mentions,
        };
        let first = message("front", "#general".to_string(), "one", 1_000);
        let second = message("front", "#general".to_string(), "two", 2_000);
        let pinned = BTreeSet::from([second.id.clone()]);
        let today = local_time(0).date_naive();
        let layout = feed_layout(
            &[first, second],
            60,
            None,
            &names,
            today,
            FeedMode::Conversation,
            &pinned,
        );
        let headers: Vec<&FeedLine> = layout
            .lines
            .iter()
            .filter(|line| line.iter().any(|(_, part)| matches!(part, Part::Agent(..))))
            .collect();
        assert_eq!(headers.len(), 2, "the pin starts a new group");
        assert!(
            headers[1]
                .iter()
                .any(|(text, part)| { *part == Part::Pin && text.contains("📌") })
        );
        assert!(!headers[0].iter().any(|(_, part)| *part == Part::Pin));
    }
}
