//! The review pane reuses the normal ACP tracker, scrollback, and composer.
use crate::{
    acp::{meta::NotificationMeta, tracker::AcpUpdateTracker},
    scrollback::{RenderBlock, ScratchBuffer, ScrollbackPane, ScrollbackState},
    theme::Theme,
    views::prompt_widget::{EnterOutcome, PromptFlag, PromptInfo, PromptStyle, PromptWidget},
};
use agent_client_protocol as acp;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Widget},
};
use std::{cell::RefCell, path::Path, rc::Rc};
use xai_grok_pager_render::render::SafeBuf;
use xai_grok_shell::extensions::review_chat::{AskRequest, AskResponse, ReviewAnchor};

pub type SharedReviewChat = Rc<RefCell<ReviewChatState>>;

pub struct ReviewExchange {
    pub anchor: ReviewAnchor,
    pub question: String,
    pub answer: String,
    pub stale: bool,
    anchor_entry: crate::scrollback::entry::EntryId,
}

#[derive(serde::Serialize)]
struct ReviewDraftContext {
    anchor: ReviewAnchor,
    question: String,
    answer: String,
}

pub struct ReviewChatState {
    pub parent_session_id: String,
    pub visible: bool,
    pub focused: bool,
    pub busy: bool,
    pub current_child_id: Option<String>,
    pub latest_child_id: Option<String>,
    /// Effective child model from the standard spawn notification (resumes pin it).
    pub model_id: Option<String>,
    pub model_name: String,
    main_model_name: String,
    pub exchanges: Vec<ReviewExchange>,
    pub prompt: PromptWidget,
    pub draft: Option<PromptWidget>,
    draft_context: Option<ReviewDraftContext>,
    draft_scrollback: ScrollbackState,
    pub message: Option<String>,
    tracker: AcpUpdateTracker,
    scrollback: ScrollbackState,
    scratch: ScratchBuffer,
    last_event_seq: Option<u64>,
    pub chat_area: Rect,
    pub cursor_pos: Option<(u16, u16)>,
    pub question_anchor: Option<ReviewAnchor>,
}

pub enum ReviewChatOutcome {
    Changed,
    Ask(AskRequest),
    Forward(String),
    Cancel(String),
}

impl ReviewChatState {
    pub fn new(parent_session_id: String, cwd: &Path) -> Self {
        let mut tracker = AcpUpdateTracker::new();
        tracker.set_session_cwd(cwd);
        Self {
            parent_session_id,
            visible: false,
            focused: false,
            busy: false,
            current_child_id: None,
            latest_child_id: None,
            model_id: None,
            model_name: "Main chat model".into(),
            main_model_name: "Main chat model".into(),
            exchanges: vec![],
            prompt: PromptWidget::new_with_cwd(cwd),
            draft: None,
            draft_context: None,
            draft_scrollback: ScrollbackState::new(),
            message: None,
            tracker,
            scrollback: ScrollbackState::new(),
            scratch: ScratchBuffer::new(),
            last_event_seq: None,
            chat_area: Rect::default(),
            cursor_pos: None,
            question_anchor: None,
        }
    }

    pub fn sync_model_name(&mut self, models: &crate::acp::model_state::ModelState) {
        self.main_model_name = models
            .current_model_name()
            .unwrap_or_else(|| "Main chat model".into());
        self.model_name = match &self.model_id {
            Some(id) => models
                .available
                .iter()
                .find(|(model_id, _)| model_id.0.as_ref() == id.as_str())
                .map(|(_, info)| info.name.clone())
                .unwrap_or_else(|| id.clone()),
            None => models
                .current_model_name()
                .unwrap_or_else(|| "Main chat model".into()),
        };
    }

    pub fn open(&mut self, anchor: Option<ReviewAnchor>) {
        self.visible = true;
        self.focused = true;
        self.question_anchor = anchor;
    }

    pub fn refresh_anchors(&mut self, hunks: &[super::changes_modal::HunkDto]) {
        for exchange in &mut self.exchanges {
            let stale = !hunks.iter().any(|h| {
                h.id == exchange.anchor.hunk_id
                    && h.review_path() == exchange.anchor.path
                    && h.patch.as_deref() == Some(exchange.anchor.patch.as_str())
            });
            if stale != exchange.stale {
                exchange.stale = stale;
                if let Some(entry) = self.scrollback.get_by_id_mut(exchange.anchor_entry) {
                    entry.block = RenderBlock::system(format!(
                        "{} · {}{}",
                        exchange.anchor.path,
                        exchange.anchor.comparison,
                        if stale { " · older diff snapshot" } else { "" }
                    ));
                    entry.invalidate_cache();
                }
                self.scrollback.mark_height_dirty(exchange.anchor_entry);
            }
        }
    }

    pub fn begin_question(&mut self, anchor: ReviewAnchor) -> Option<AskRequest> {
        if self.busy {
            return None;
        }
        let question = self.prompt.try_send()?;
        if question.trim().is_empty() {
            return None;
        }
        self.prompt.set_text("");
        let child_id = uuid::Uuid::now_v7().to_string();
        self.tracker.finish_turn(&mut self.scrollback);
        self.last_event_seq = None;
        let anchor_entry = self.scrollback.push_block(RenderBlock::system(format!(
            "{} · {}",
            anchor.path, anchor.comparison
        )));
        self.scrollback
            .push_block(RenderBlock::user_prompt(question.clone()));
        self.scrollback.goto_bottom();
        self.exchanges.push(ReviewExchange {
            anchor: anchor.clone(),
            question: question.clone(),
            answer: String::new(),
            stale: false,
            anchor_entry,
        });
        self.current_child_id = Some(child_id.clone());
        self.busy = true;
        self.message = None;
        Some(AskRequest {
            session_id: self.parent_session_id.clone(),
            child_id,
            previous_child_id: self.latest_child_id.clone(),
            question,
            anchor,
        })
    }

    /// Undo a locally prepared question when transport/session gating refuses it.
    pub fn refuse_question(&mut self, child_id: &str, message: &str) {
        if self.current_child_id.as_deref() != Some(child_id) {
            return;
        }
        self.tracker.finish_turn(&mut self.scrollback);
        if let Some(exchange) = self.exchanges.pop() {
            let ids: Vec<_> = self
                .scrollback
                .iter_entries()
                .filter(|(id, _)| id.value() >= exchange.anchor_entry.value())
                .map(|(id, _)| id)
                .collect();
            for id in ids {
                self.scrollback.remove_entry(id);
            }
            self.prompt.set_text(&exchange.question);
        }
        self.current_child_id = None;
        self.busy = false;
        self.last_event_seq = None;
        self.message = Some(message.into());
    }

    pub fn receive_update(&mut self, update: acp::SessionUpdate, meta: &NotificationMeta) {
        if meta
            .event_seq
            .is_some_and(|seq| self.last_event_seq.is_some_and(|last| seq <= last))
        {
            return;
        }
        self.last_event_seq = meta.event_seq.or(self.last_event_seq);
        // The child prompt contains internal diff context. The pane already
        // painted the human question, so suppress the child's prompt echo.
        if matches!(update, acp::SessionUpdate::UserMessageChunk(_)) {
            return;
        }
        if let acp::SessionUpdate::AgentMessageChunk(chunk) = &update
            && let acp::ContentBlock::Text(text) = &chunk.content
            && let Some(exchange) = self.exchanges.last_mut()
        {
            exchange.answer.push_str(&text.text);
        }
        self.tracker
            .handle_update(update, meta, &mut self.scrollback);
    }

    pub fn finish(&mut self, response: Result<AskResponse, String>) {
        match response {
            Ok(response) => {
                if self.current_child_id.as_deref() != Some(&response.child_id) {
                    return;
                }
                if let Some(exchange) = self.exchanges.last_mut()
                    && exchange.answer != response.output
                    && !response.output.is_empty()
                {
                    // The completed output also repairs gaps in unsequenced or
                    // interrupted streams. Replace only this question's answer
                    // blocks; retain its tools, thoughts, and earlier exchanges.
                    self.tracker.finish_turn(&mut self.scrollback);
                    let ids: Vec<_> = self
                        .scrollback
                        .iter_entries()
                        .filter(|(id, entry)| {
                            id.value() > exchange.anchor_entry.value()
                                && matches!(entry.block, RenderBlock::AgentMessage(_))
                        })
                        .map(|(id, _)| id)
                        .collect();
                    for id in ids {
                        self.scrollback.remove_entry(id);
                    }
                    exchange.answer = response.output.clone();
                    self.scrollback
                        .push_block(RenderBlock::agent_message(response.output));
                }
                if response.success {
                    self.latest_child_id = Some(response.child_id);
                }
                self.message = response.error;
            }
            Err(error) => self.message = Some(error),
        }
        self.tracker.finish_turn(&mut self.scrollback);
        self.busy = false;
    }

    pub fn open_draft(&mut self) {
        if self.draft.is_some() {
            self.visible = true;
            self.focused = true;
            return;
        }
        if self.busy {
            self.message = Some("Wait for the answer or stop the question first.".into());
            return;
        }
        let Some(exchange) = self.exchanges.last() else {
            self.message = Some("Ask a review question first.".into());
            return;
        };
        self.draft = Some(PromptWidget::new());
        self.draft_context = Some(ReviewDraftContext {
            anchor: exchange.anchor.clone(),
            question: exchange.question.clone(),
            answer: exchange.answer.clone(),
        });
        self.draft_scrollback = ScrollbackState::new();
        self.draft_scrollback
            .push_block(RenderBlock::system(format!(
                "{} · {}",
                exchange.anchor.path, exchange.anchor.comparison
            )));
        self.draft_scrollback
            .push_block(RenderBlock::user_prompt(exchange.question.clone()));
        self.draft_scrollback
            .push_block(RenderBlock::agent_message(exchange.answer.clone()));
        self.draft_scrollback
            .push_block(RenderBlock::system(exchange.anchor.patch.clone()));
        self.draft_scrollback.goto_top();
        self.question_anchor = Some(exchange.anchor.clone());
        self.visible = true;
        self.focused = true;
        self.message = None;
    }

    /// Clear the handoff only after the main dispatcher accepts it.
    pub fn handoff_accepted(&mut self) {
        self.draft = None;
        self.draft_context = None;
        self.message = Some("Sent to main chat".into());
    }

    pub fn handle_key(
        &mut self,
        key: &KeyEvent,
        anchor: Option<ReviewAnchor>,
    ) -> ReviewChatOutcome {
        match key.code {
            KeyCode::Esc => {
                if self.draft.take().is_some() {
                    self.draft_context = None;
                }
                self.focused = false;
                return ReviewChatOutcome::Changed;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focused = false;
                return ReviewChatOutcome::Changed;
            }
            KeyCode::PageUp => {
                if self.draft.is_some() {
                    let step = self
                        .draft_scrollback
                        .scroll_info()
                        .1
                        .saturating_sub(1)
                        .max(1);
                    self.draft_scrollback.scroll_up(step);
                } else {
                    self.scrollback.scroll_up(8);
                }
                return ReviewChatOutcome::Changed;
            }
            KeyCode::PageDown => {
                if self.draft.is_some() {
                    let step = self
                        .draft_scrollback
                        .scroll_info()
                        .1
                        .saturating_sub(1)
                        .max(1);
                    self.draft_scrollback.scroll_down(step);
                } else {
                    self.scrollback.scroll_down(8);
                }
                return ReviewChatOutcome::Changed;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_draft();
                return ReviewChatOutcome::Changed;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) && self.busy => {
                if let Some(id) = &self.current_child_id {
                    self.message = Some("Stopping question…".into());
                    return ReviewChatOutcome::Cancel(id.clone());
                }
            }
            KeyCode::Char('s')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.draft.is_some() =>
            {
                let text = self.draft.as_ref().unwrap().text().trim().to_owned();
                if text.is_empty() {
                    self.message = Some("Enter a message.".into());
                    return ReviewChatOutcome::Changed;
                }
                let context =
                    serde_json::to_string(self.draft_context.as_ref().unwrap()).unwrap_or_default();
                return ReviewChatOutcome::Forward(format!(
                    "{text}\n\nAttached review context (reference only):\n{context}\n\nInspect current code before editing; this snapshot may be stale."
                ));
            }
            _ => {}
        }
        if let Some(draft) = &mut self.draft {
            // Draft editing is multiline; only explicit Ctrl+S sends it.
            self.message = None;
            draft.handle_key(key);
        } else {
            match self.prompt.route_enter(key) {
                EnterOutcome::NewlineInserted => return ReviewChatOutcome::Changed,
                EnterOutcome::PassThrough => {
                    self.prompt.handle_key(key);
                    return ReviewChatOutcome::Changed;
                }
                EnterOutcome::Submit => {}
            }
            let using_snapshot = anchor.is_none();
            let anchor = anchor.or_else(|| self.exchanges.last().map(|e| e.anchor.clone()));
            if let Some(anchor) = anchor
                && let Some(req) = self.begin_question(anchor)
            {
                if using_snapshot {
                    self.refresh_anchors(&[]);
                }
                return ReviewChatOutcome::Ask(req);
            }
            if self.busy {
                self.message = Some("A question is already running.".into());
            }
        }
        ReviewChatOutcome::Changed
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(draft) = &mut self.draft {
            draft.handle_paste(text);
        } else {
            self.prompt.handle_paste(text);
        }
    }

    fn render_draft(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        let context_h = (area.height / 2).max(3).min(area.height.saturating_sub(5));
        let context_area = Rect::new(area.x, area.y, area.width, context_h);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(theme.gray_dim))
            .title(" Attached context (read-only) ");
        let mut viewport = block.inner(context_area);
        viewport.width = viewport.width.saturating_sub(1);
        block.render(context_area, buf);
        self.draft_scrollback
            .prepare_layout(viewport.width, viewport.height);
        let output = ScrollbackPane::new().active(true).render_with_scratch(
            viewport,
            buf,
            &self.draft_scrollback,
            &mut self.scratch,
        );
        super::agent::render_scrollbar(
            buf,
            viewport,
            viewport.right(),
            &crate::appearance::ScrollbarConfig::default(),
            output.scroll_info,
            false,
            theme,
        );
        if let Some(message) = &self.message {
            buf.set_line_safe(
                area.x,
                context_area.bottom(),
                &Line::styled(message, Style::default().fg(theme.warning)),
                area.width,
            );
        }
        let editor_area = Rect::new(
            area.x,
            context_area.bottom() + 1,
            area.width,
            area.height.saturating_sub(context_h + 1),
        );
        let style = PromptStyle {
            focused: self.focused,
            show_prefix: false,
            chrome_pad_left: 1,
            chrome_pad_right: 1,
            title: Some("Message to main AI".into()),
            placeholder_override: Some("Message…".into()),
            placeholder_when_focused: true,
            ..Default::default()
        };
        let info = PromptInfo {
            model_name: &self.main_model_name,
            ..Default::default()
        };
        self.cursor_pos = self
            .draft
            .as_mut()
            .unwrap()
            .draw(buf, editor_area, None, &style, Some(&info), None)
            .cursor_pos;
    }

    pub fn render(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        self.chat_area = area;
        self.cursor_pos = None;
        if area.width < 6 || area.height < 9 {
            return;
        }
        if self.draft.is_some() {
            self.render_draft(buf, area, theme);
            return;
        }
        let title = "Review chat · read-only";
        buf.set_line_safe(
            area.x,
            area.y,
            &Line::styled(title, Style::default().fg(theme.accent_user)),
            area.width,
        );
        let editor_h = 4;
        let viewport = Rect::new(
            area.x,
            area.y + 2,
            area.width.saturating_sub(1),
            area.height.saturating_sub(editor_h + 4),
        );
        if !self.exchanges.is_empty() {
            self.scrollback
                .prepare_layout(viewport.width, viewport.height);
            let output = ScrollbackPane::new()
                .active(self.focused)
                .render_with_scratch(viewport, buf, &self.scrollback, &mut self.scratch);
            super::agent::render_scrollbar(
                buf,
                viewport,
                area.right() - 1,
                &crate::appearance::ScrollbarConfig::default(),
                output.scroll_info,
                false,
                theme,
            );
        }
        let anchor_label = self
            .question_anchor
            .as_ref()
            .or_else(|| self.exchanges.last().map(|e| &e.anchor))
            .map(|a| a.path.as_str())
            .unwrap_or("No hunk selected");
        buf.set_line_safe(
            area.x,
            area.y + 1,
            &Line::styled(anchor_label, Style::default().fg(theme.gray)),
            area.width,
        );
        let stale = self.exchanges.iter().any(|e| e.stale);
        let status = self.message.as_deref().or(if self.busy {
            Some("Thinking…")
        } else if stale {
            Some("Earlier diff snapshot")
        } else {
            None
        });
        if let Some(status) = status {
            buf.set_line_safe(
                area.x,
                area.bottom().saturating_sub(editor_h + 1),
                &Line::styled(status, Style::default().fg(theme.gray)),
                area.width,
            );
        }
        let editor_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(editor_h),
            area.width,
            editor_h,
        );
        let style = PromptStyle {
            focused: self.focused,
            chrome_pad_left: 1,
            chrome_pad_right: 1,
            title: Some("Message to review AI".into()),
            placeholder_override: Some("Message…".into()),
            placeholder_when_focused: true,
            ..Default::default()
        };
        let flags = [PromptFlag {
            text: "from main",
            color: None,
            bold: false,
        }];
        let info = PromptInfo {
            model_name: &self.model_name,
            flags: &flags,
            ..Default::default()
        };
        let rendered = self
            .prompt
            .draw(buf, editor_area, None, &style, Some(&info), None);
        self.cursor_pos = rendered.cursor_pos;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn anchor() -> ReviewAnchor {
        ReviewAnchor {
            path: "src/sample.py".into(),
            hunk_id: "h1".into(),
            patch: "+new\n-old".into(),
            comparison: "Session baseline → working files".into(),
        }
    }
    #[test]
    fn long_transcript_scrolls_without_clipping_question_or_draft_border() {
        let mut state = ReviewChatState::new("parent".into(), Path::new("."));
        state.open(Some(anchor()));
        state.prompt.set_text("Explain the changes");
        let request = state.begin_question(anchor()).unwrap();
        state.finish(Ok(AskResponse {
            child_id: request.child_id,
            success: true,
            output: (0..100)
                .map(|i| format!("Review explanation {i:03}\n"))
                .collect(),
            error: None,
        }));
        let area = Rect::new(2, 3, 82, 15);
        let theme = Theme::current();
        let mut buf = Buffer::empty(Rect::new(0, 0, 90, 24));
        state.render(&mut buf, area, &theme);
        let initial = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        let initial_offset = state.scrollback.scroll_info().0;
        assert!(initial_offset > 0);
        state.handle_key(&KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), None);
        buf.reset();
        state.render(&mut buf, area, &theme);
        assert!(state.scrollback.scroll_info().0 < initial_offset);
        let scrolled = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert_ne!(initial, scrolled);
        for drafting in [false, true] {
            if drafting {
                state.open_draft();
                state.draft.as_mut().unwrap().set_text(
                    "First request line\nSecond line\nThird line\nFourth line\nLast line",
                );
            }
            buf.reset();
            state.render(&mut buf, area, &theme);
            if drafting {
                let form = buf.content.iter().map(|c| c.symbol()).collect::<String>();
                assert!(form.contains("Message to main AI"));
                assert!(form.contains("Attached context (read-only)"));
                assert!(!form.contains("Requested change:"));
                let before = state.draft_scrollback.scroll_info().0;
                let message = state.draft.as_ref().unwrap().text().to_owned();
                state.handle_key(&KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), None);
                state.render(&mut buf, area, &theme);
                assert!(state.draft_scrollback.scroll_info().0 > before);
                assert_eq!(state.draft.as_ref().unwrap().text(), message);
            }
            assert_eq!(buf[(area.x, area.bottom() - 1)].symbol(), "╰");
            assert_eq!(buf[(area.right() - 1, area.bottom() - 1)].symbol(), "╯");
            let (cursor_x, cursor_y) = state.cursor_pos.unwrap();
            assert!(cursor_x > area.x && cursor_x < area.right() - 1);
            assert!(cursor_y < area.bottom() - 1);
        }
    }

    #[test]
    fn handoff_requires_explicit_send_and_retains_anchor() {
        let mut state = ReviewChatState::new("parent".into(), Path::new("."));
        state.prompt.set_text("Please explain this check");
        let req = state.begin_question(anchor()).unwrap();
        state.finish(Ok(AskResponse {
            child_id: req.child_id.clone(),
            success: true,
            output: "It validates the path.".into(),
            error: None,
        }));
        state.open_draft();
        assert!(state.draft.as_ref().unwrap().text().is_empty());
        assert!(matches!(
            state.handle_key(
                &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                None
            ),
            ReviewChatOutcome::Changed
        ));
        assert!(state.draft.is_some());
        state
            .draft
            .as_mut()
            .unwrap()
            .set_text("Keep the validation but improve the message.");
        assert!(matches!(
            state.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), None),
            ReviewChatOutcome::Changed
        ));
        let ReviewChatOutcome::Forward(text) = state.handle_key(
            &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            None,
        ) else {
            panic!("expected explicit send")
        };
        assert!(text.starts_with("Keep the validation but improve the message."));
        let attachment = text
            .split("Attached review context (reference only):\n")
            .nth(1)
            .unwrap()
            .split("\n\nInspect current code")
            .next()
            .unwrap();
        let attachment: serde_json::Value = serde_json::from_str(attachment).unwrap();
        assert_eq!(attachment["question"], "Please explain this check");
        assert_eq!(attachment["answer"], "It validates the path.");
        assert_eq!(attachment["anchor"]["path"], "src/sample.py");
        assert!(text.contains("src/sample.py"));
        assert!(text.contains("Keep the validation"));
        assert!(text.contains("Inspect current code"));
        assert_eq!(state.latest_child_id, Some(req.child_id));
    }
    #[test]
    fn followups_resume_and_stale_answers_keep_the_original_anchor() {
        let mut state = ReviewChatState::new("parent".into(), Path::new("."));
        state.prompt.set_text("Why?");
        let first = state.begin_question(anchor()).unwrap();
        state.finish(Ok(AskResponse {
            child_id: first.child_id.clone(),
            success: true,
            output: "Because it validates paths.".into(),
            error: None,
        }));
        state.refresh_anchors(&[]);
        assert!(state.exchanges[0].stale);
        assert_eq!(state.exchanges[0].anchor, anchor());
        state.prompt.set_text("What about empty paths?");
        let second = state.begin_question(anchor()).unwrap();
        assert_eq!(second.previous_child_id, Some(first.child_id));
        state.prompt.set_text("A third question");
        assert!(state.begin_question(anchor()).is_none());
        state.finish(Err("Disconnected".into()));
        assert!(!state.busy);
        assert!(state.begin_question(anchor()).is_some());
    }
    #[test]
    fn replayed_review_chunks_deduplicate_and_final_output_repairs_gaps() {
        let mut state = ReviewChatState::new("parent".into(), Path::new("."));
        state.prompt.set_text("Why?");
        let req = state.begin_question(anchor()).unwrap();
        let chunk = |text: &str| {
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            )))
        };
        for (text, seq, replay) in [
            ("First ", 1, false),
            ("First ", 1, true),
            ("middle ", 2, true),
            ("last.", 3, false),
        ] {
            state.receive_update(
                chunk(text),
                &NotificationMeta {
                    event_seq: Some(seq),
                    is_replay: replay,
                    ..Default::default()
                },
            );
        }
        assert_eq!(state.exchanges[0].answer, "First middle last.");
        // Simulate a missing event that never made the replay buffer.
        state.finish(Ok(AskResponse {
            child_id: req.child_id,
            success: true,
            output: "First middle recovered last.".into(),
            error: None,
        }));
        assert_eq!(state.exchanges[0].answer, "First middle recovered last.");
        let answers: Vec<_> = state
            .scrollback
            .iter_entries()
            .filter_map(|(_, entry)| match &entry.block {
                RenderBlock::AgentMessage(_) => entry.block.copy_text(false),
                _ => None,
            })
            .collect();
        assert_eq!(answers, ["First middle recovered last."]);
        state.open_draft();
        assert_eq!(state.draft_context.as_ref().unwrap().answer, answers[0]);
    }
    #[test]
    fn review_backslash_enter_inserts_exactly_one_newline() {
        let mut state = ReviewChatState::new("parent".into(), Path::new("."));
        state.prompt.set_text("first line\\");
        state.prompt.set_cursor(state.prompt.text().len());
        assert!(matches!(
            state.handle_key(
                &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some(anchor())
            ),
            ReviewChatOutcome::Changed
        ));
        assert_eq!(state.prompt.text(), "first line\n");
        assert!(!state.busy);
        assert!(state.exchanges.is_empty());
    }
}
