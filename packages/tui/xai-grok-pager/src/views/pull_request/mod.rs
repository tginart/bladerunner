//! Browse a captured GitHub pull request and ask about one selected excerpt.
//! Input is a snapshot, terminal events, and a session-owned review conversation.
//! Output is a read-only view and explicit review chat actions.

pub mod github;
pub mod summary;
use summary::Summary;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Paragraph, Widget, Wrap},
};
use xai_grok_shell::extensions::review_chat::ReviewAnchor;

use super::modal_window::{
    self as mw, ModalSizing, ModalWindowConfig, ModalWindowOutcome, ModalWindowState, Shortcut,
};
use super::review_chat::{ReviewChatOutcome, SharedReviewChat};
use crate::theme::Theme;
use github::{Item, Snapshot};

pub enum Outcome {
    Changed,
    Close,
    Chat(ReviewChatOutcome),
}

/// One open viewer. The request ID prevents a late fetch from replacing a newer view.
pub struct Viewer {
    pub summary: Summary,
    pub target: String,
    pub request_id: uuid::Uuid,
    pub chat: Option<SharedReviewChat>,
    pub data: Option<Snapshot>,
    pub message: String,
    window: ModalWindowState,
    selected: usize,
    part: usize,
    scroll: usize,
    first_row: usize,
    list_area: Rect,
    detail_area: Rect,
    cached_lines: Option<(u16, Vec<Line<'static>>)>,
}

impl Viewer {
    pub fn loading() -> Self {
        Self {
            summary: Summary::default(),
            target: String::new(),
            request_id: uuid::Uuid::new_v4(),
            chat: None,
            data: None,
            message: "Loading pull request from GitHub…".into(),
            window: ModalWindowState::new(),
            selected: 0,
            part: 0,
            scroll: 0,
            first_row: 0,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            cached_lines: None,
        }
    }

    pub fn key(&mut self, key: &KeyEvent) -> Outcome {
        if let Some(shared) = &self.chat
            && shared.borrow().focused
        {
            let mut chat = shared.borrow_mut();
            let anchor = (key.code == KeyCode::Enter)
                .then(|| chat.question_anchor.clone())
                .flatten();
            let outcome = chat.handle_key(key, anchor);

            return Outcome::Chat(outcome);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Right | KeyCode::Tab => self.tab((self.window.active_tab + 1) % 3),
            KeyCode::Left | KeyCode::BackTab => self.tab((self.window.active_tab + 2) % 3),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected.saturating_add(1)),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Char('[') => self.hunk(self.part.saturating_sub(1)),
            KeyCode::Char(']') => self.hunk(self.part.saturating_add(1)),
            KeyCode::PageDown => self.scroll_by(self.detail_area.height.max(1) as isize),
            KeyCode::PageUp => self.scroll_by(-(self.detail_area.height.max(1) as isize)),
            KeyCode::Char('?') => self.ask(),
            KeyCode::Char('d') => return self.digest(),
            _ => {}
        }
        Outcome::Changed
    }

    pub fn mouse(&mut self, mouse: &MouseEvent) -> Outcome {
        let point = (mouse.column, mouse.row).into();
        if let Some(shared) = &self.chat
            && shared.borrow().visible
            && shared.borrow().chat_area.contains(point)
        {
            let mut chat = shared.borrow_mut();
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    chat.focused = true;
                    if let Some(draft) = &mut chat.draft {
                        draft.handle_mouse(mouse);
                    } else {
                        chat.prompt.handle_mouse(mouse);
                    }
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let code = if mouse.kind == MouseEventKind::ScrollUp {
                        KeyCode::PageUp
                    } else {
                        KeyCode::PageDown
                    };
                    chat.handle_key(&KeyEvent::new(code, KeyModifiers::NONE), None);
                }
                _ => {}
            }
            return Outcome::Changed;
        }
        match mw::handle_modal_mouse(&mut self.window, mouse.kind, mouse.column, mouse.row) {
            ModalWindowOutcome::CloseRequested => return Outcome::Close,
            ModalWindowOutcome::TabChanged(tab) => self.tab(tab),
            ModalWindowOutcome::ShortcutActivated(1) => self.ask(),
            ModalWindowOutcome::ShortcutActivated(2) => return Outcome::Close,
            ModalWindowOutcome::ShortcutActivated(3) => self.defocus_chat(),
            ModalWindowOutcome::ShortcutActivated(4) => return self.digest(),
            ModalWindowOutcome::Unhandled if self.list_area.contains(point) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.select(self.first_row + usize::from(mouse.row - self.list_area.y))
                }
                MouseEventKind::ScrollUp => self.select(self.selected.saturating_sub(1)),
                MouseEventKind::ScrollDown => self.select(self.selected.saturating_add(1)),
                _ => {}
            },
            ModalWindowOutcome::Unhandled if self.detail_area.contains(point) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_by(-3),
                MouseEventKind::ScrollDown => self.scroll_by(3),
                _ => {}
            },
            _ => {}
        }
        Outcome::Changed
    }

    pub fn render(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        let title = self
            .data
            .as_ref()
            .map_or("Pull request", |pr| pr.title.as_str());
        let tabs = ["Overview", "Files", "Discussion"];
        let chat_open = self.chat.as_ref().is_some_and(|chat| chat.borrow().visible);
        let hints = if self.chat.as_ref().is_some_and(|chat| chat.borrow().focused) {
            if self
                .chat
                .as_ref()
                .is_some_and(|chat| chat.borrow().draft.is_some())
            {
                vec![
                    ("Ctrl+S send to main", 0),
                    ("Esc cancel draft", 0),
                    ("Tab back", 3),
                ]
            } else {
                vec![("Enter send", 0), ("PgUp/Dn chat", 0), ("Tab back", 3)]
            }
        } else if self.data.is_none() {
            vec![("Esc summary", 2)]
        } else {
            vec![
                ("←→ tabs", 0),
                ("↑↓ select", 0),
                ("[ ] hunks", 0),
                ("PgUp/Dn scroll", 0),
                ("d AI digest", 4),
                ("? Ask AI", 1),
                ("Esc summary", 2),
            ]
        };
        let shortcuts: Vec<_> = hints
            .into_iter()
            .map(|(label, id)| Shortcut {
                label: label.into(),
                clickable: id != 0,
                id,
            })
            .collect();
        let config = ModalWindowConfig {
            title,
            tabs: self.data.as_ref().map(|_| tabs.as_slice()),
            shortcuts: &shortcuts,
            sizing: ModalSizing {
                width_pct: 1.0,
                max_width: u16::MAX,
                v_margin: 0,
                v_pad: 0,
                ..ModalSizing::large()
            },
            fold_info: None,
        };
        self.list_area = Rect::default();
        self.detail_area = Rect::default();
        let Some(content) = mw::render_modal_window(buf, area, &mut self.window, &config, theme)
        else {
            return;
        };
        let mut area = content.content;
        if self.data.is_none() {
            Paragraph::new(self.message.as_str())
                .wrap(Wrap { trim: false })
                .render(area, buf);
            return;
        }
        if chat_open && let Some(shared) = &self.chat {
            let (viewer, chat) = if area.width >= 96 {
                let [viewer, chat] =
                    Layout::horizontal([Constraint::Percentage(60), Constraint::Fill(1)])
                        .spacing(1)
                        .areas(area);
                (viewer, chat)
            } else if area.height >= 20 {
                let [viewer, chat] =
                    Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)])
                        .spacing(1)
                        .areas(area);
                (viewer, chat)
            } else if shared.borrow().focused {
                shared.borrow_mut().render(buf, area, theme);
                return;
            } else {
                (area, Rect::default())
            };
            shared.borrow_mut().render(buf, chat, theme);
            area = viewer;
        }
        if self.window.active_tab != 0 {
            area = self.render_list(buf, area, theme);
        }
        self.render_detail(buf, area, theme);
    }

    fn render_list(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) -> Rect {
        let [list, detail] = if area.width >= 68 {
            Layout::horizontal([Constraint::Percentage(33), Constraint::Fill(1)])
                .spacing(1)
                .areas(area)
        } else {
            Layout::vertical([
                Constraint::Length((area.height / 3).min(6)),
                Constraint::Fill(1),
            ])
            .spacing(1)
            .areas(area)
        };
        self.list_area = list;
        let h = usize::from(list.height.max(1));
        self.first_row = self
            .first_row
            .min(self.selected)
            .max((self.selected + 1).saturating_sub(h));
        for (y, item) in self.items().iter().enumerate().skip(self.first_row).take(h) {
            let style = if self.selected == y {
                Style::default()
                    .fg(theme.accent_user)
                    .bg(theme.bg_highlight)
            } else {
                Style::default().fg(theme.text_secondary)
            };
            let row = Rect::new(list.x, list.y + (y - self.first_row) as u16, list.width, 1);
            Paragraph::new(item.label.as_str())
                .style(style)
                .render(row, buf);
        }
        detail
    }

    fn render_detail(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        let Some(item) = self.item() else {
            Paragraph::new("No entries. See Overview for any loading errors.")
                .wrap(Wrap { trim: false })
                .render(area, buf);
            return;
        };
        let mut heading = item.location.clone();
        if item.parts.len() > 1 {
            heading.push_str(&format!(" · hunk {}/{}", self.part + 1, item.parts.len()));
        }
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        Paragraph::new(heading)
            .style(Style::default().fg(theme.accent_user))
            .render(header, buf);
        if body.is_empty() {
            return;
        }
        if self
            .cached_lines
            .as_ref()
            .is_none_or(|(width, _)| *width != body.width)
        {
            let text = &item.parts[self.part];
            let lines = if self.window.active_tab == 0 {
                use crate::scrollback::blocks::markdown_content::MarkdownContent;
                MarkdownContent::new(text.clone())
                    .output(usize::from(body.width))
                    .lines
                    .into_iter()
                    .map(|line| line.content)
                    .collect()
            } else {
                patch_lines(text, body.width, theme)
            };
            self.cached_lines = Some((body.width, lines));
        }
        let lines = &self
            .cached_lines
            .as_ref()
            .expect("cached at viewport width")
            .1;
        self.scroll = self
            .scroll
            .min(lines.len().saturating_sub(usize::from(body.height)));
        let visible = lines
            .iter()
            .skip(self.scroll)
            .take(usize::from(body.height))
            .cloned()
            .collect::<Vec<_>>();
        Paragraph::new(visible).render(body, buf);
        self.detail_area = body;
    }

    fn items(&self) -> &[Item] {
        let Some(data) = &self.data else {
            return &[];
        };
        match self.window.active_tab {
            0 => std::slice::from_ref(&data.overview),
            1 => &data.files,
            _ => &data.discussions,
        }
    }

    fn item(&self) -> Option<&Item> {
        self.items().get(self.selected)
    }

    fn tab(&mut self, tab: usize) {
        self.window.active_tab = tab;
        self.first_row = 0;
        self.select(0);
    }

    fn defocus_chat(&mut self) {
        if let Some(chat) = &self.chat {
            let mut chat = chat.borrow_mut();
            chat.focused = false;
        }
    }

    fn select(&mut self, index: usize) {
        self.defocus_chat();
        self.selected = index.min(self.items().len().saturating_sub(1));
        self.hunk(0);
    }

    fn hunk(&mut self, index: usize) {
        self.part = index.min(self.item().map_or(0, |i| i.parts.len() - 1));
        self.cached_lines = None;
        self.scroll = 0;
        self.update_chat_anchor();
    }

    fn scroll_by(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines);
    }

    pub fn enter_fullscreen(&mut self) {
        if let Some(chat) = &self.chat {
            let mut chat = chat.borrow_mut();
            chat.visible = true;
            chat.focused = false;
        }
        self.update_chat_anchor();
    }

    fn update_chat_anchor(&self) {
        if let Some(chat) = &self.chat
            && chat.borrow().draft.is_none()
        {
            chat.borrow_mut().question_anchor = self.anchor(self.window.active_tab == 0);
        }
    }

    fn ask(&mut self) {
        if let Some(chat) = &self.chat {
            chat.borrow_mut().focused = true;
            chat.borrow_mut().visible = true;
        }
    }

    fn digest(&mut self) -> Outcome {
        let (Some(chat), Some(anchor)) = (&self.chat, self.anchor(true)) else {
            return Outcome::Changed;
        };
        let mut chat = chat.borrow_mut();
        chat.focused = true;
        chat.visible = true;
        if chat.busy || chat.draft.is_some() {
            chat.message = Some("Finish the current review question or draft first.".into());
            return Outcome::Changed;
        }
        let question = "Give me a short review digest of this PR. Explain its intent, the important changes, what reviewers have already settled, and what still needs attention. Cite files or threads. Identify gaps in the supplied excerpts.";
        let request = chat
            .begin_request(anchor, question.into())
            .expect("idle review chat");
        Outcome::Chat(ReviewChatOutcome::Ask(request))
    }

    fn anchor(&self, overview: bool) -> Option<ReviewAnchor> {
        let pr = self.data.as_ref()?;
        let item = if overview { &pr.overview } else { self.item()? };
        Some(ReviewAnchor {
            path: if !overview && self.window.active_tab == 1 {
                item.label.clone()
            } else {
                item.location.clone()
            },
            hunk_id: if overview {
                "overview".into()
            } else {
                format!("{}:{}:{}", self.window.active_tab, self.selected, self.part)
            },
            comparison: format!(
                "{} · {} · head {} (GitHub snapshot; the local checkout can differ)",
                pr.title, pr.url, pr.head
            ),
            patch: if overview {
                pr.overview_context()
            } else {
                format!(
                    "{}\n{}\n\n{}",
                    item.label, item.location, item.parts[self.part]
                )
            },
        })
    }
}

/// Wrap once at the current viewport width. Keep diff colors on continuation lines.
fn patch_lines(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let color = if line.starts_with('+') {
            theme.accent_success
        } else if line.starts_with('-') {
            theme.accent_error
        } else if line.starts_with("@@") {
            theme.accent_user
        } else {
            theme.text_primary
        };
        for wrapped in textwrap::wrap(line, usize::from(width)) {
            lines.push(Line::styled(
                wrapped.into_owned(),
                Style::default().fg(color),
            ));
        }
    }
    lines
}
