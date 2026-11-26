/*
Purpose: Connect the session file browser to the agent view.
Input: The session directory, input events, and file worker responses.
Output: Browser state changes and native file open requests.
*/

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::{AgentPane, AgentView};
use crate::app::app_view::InputOutcome;
use crate::views::agent::AgentViewLayout;
use crate::views::file_browser::{BrowserAction, FileBrowserState, split_area};

impl AgentView {
    /*
    Purpose: Open the browser for this session and retain a live editor.
    Input: The current session directory and screen mode.
    Output: A focused browser, or a notice when it cannot open.
    */
    pub(crate) fn open_file_browser(&mut self) {
        if self.is_minimal_mode() {
            self.show_toast("Use /fullscreen to browse files");
            return;
        }
        let needs_new = self
            .file_browser
            .as_ref()
            .is_none_or(|browser| browser.root() != self.session.cwd && !browser.has_editor());
        if needs_new {
            match FileBrowserState::new(&self.session.cwd) {
                Ok(browser) => self.file_browser = Some(browser),
                Err(error) => {
                    self.show_toast(&format!("Could not open Files: {error}"));
                    return;
                }
            }
        }
        if let Some(browser) = &mut self.file_browser {
            browser.visible = true;
            browser.focused = true;
        }
    }

    pub(crate) fn file_browser_is_open(&self) -> bool {
        self.file_browser
            .as_ref()
            .is_some_and(|browser| browser.visible)
    }

    /*
    Purpose: Yield the browser area to surfaces that own higher-priority input.
    Input: The current agent overlay state.
    Output: True when browser input and frame content are available.
    */
    pub(crate) fn file_browser_can_draw(&self) -> bool {
        self.file_browser_is_open()
            && self.no_input_overlay_pending()
            && self.hub_browser.is_none()
            && self.active_modal.is_none()
            && self.extensions_modal.is_none()
            && self.agents_modal.is_none()
            && self.persona_detail.is_none()
            && self.line_viewer.is_none()
            && self.block_viewer.is_none()
            && self.image_viewer.is_none()
            && self.video_viewer.is_none()
            && self.gboom.is_none()
            && self.inline_edit.is_none()
            && self.jump_state.is_none()
            && self.rewind_state.is_none()
            && self.scrollback_search.is_none()
            && !self.show_goal_detail
            && !self.show_workflows
    }

    /*
    Purpose: Give the browser its part of the conversation area for this frame.
    Input: The frame layout after the search rows are reserved.
    Output: The browser area, or None when the browser does not draw.
    Note: A split layout confines the conversation to the rest of the area.
    */
    pub(super) fn place_file_browser(&self, layout: &mut AgentViewLayout) -> Option<Rect> {
        if !self.file_browser_can_draw() {
            return None;
        }
        let browser = self.file_browser.as_ref()?;
        let (browser_area, conversation) = split_area(
            browser.layout(),
            layout.scrollback,
            layout.scrollbar_gutter(),
        );
        if let Some(conversation) = conversation {
            layout.confine_scrollback(conversation);
        }
        Some(browser_area)
    }

    pub(crate) fn poll_file_browser(&mut self) -> bool {
        if self
            .file_browser
            .as_ref()
            .is_some_and(|browser| browser.root() != self.session.cwd && !browser.has_editor())
        {
            self.file_browser = None;
            return true;
        }
        self.file_browser
            .as_mut()
            .is_some_and(FileBrowserState::poll)
    }

    pub(crate) fn file_browser_needs_tick(&self) -> bool {
        self.file_browser
            .as_ref()
            .is_some_and(|browser| browser.needs_tick() || browser.root() != self.session.cwd)
    }

    /*
    Purpose: Route focused browser input and apply explicit host actions.
    Input: A host input event.
    Output: An input outcome, or no outcome when another surface owns the event.
    */
    pub(super) fn handle_file_browser_input(&mut self, event: &Event) -> Option<InputOutcome> {
        if !self.file_browser_can_draw() {
            return None;
        }
        let browser = self.file_browser.as_mut()?;
        let action = match event {
            Event::Key(key) if key.kind == KeyEventKind::Release => return None,
            Event::Key(key)
                if key.code == KeyCode::Char('q')
                    && key.modifiers == KeyModifiers::CONTROL
                    && !browser.terminal_has_focus() =>
            {
                return None;
            }
            Event::Key(key) if browser.focused => browser.handle_key(key),
            Event::Key(key) if key.code == KeyCode::Tab && !self.prompt.any_dropdown_open() => {
                browser.focus_tree();
                return Some(InputOutcome::Changed);
            }
            Event::Mouse(mouse) if browser.contains(mouse.column, mouse.row) => {
                browser.handle_mouse(mouse.kind, mouse.column, mouse.row)
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                browser.focused = false;
                return None;
            }
            Event::Paste(text) if browser.focused => {
                browser.handle_paste(text);
                return Some(InputOutcome::Changed);
            }
            _ => return None,
        };
        match action {
            BrowserAction::Changed => {}
            BrowserAction::Close => {
                browser.visible = false;
                browser.focused = false;
            }
            BrowserAction::FocusPrompt => {
                if self.set_active_pane(AgentPane::Prompt, false)
                    && let Some(browser) = &mut self.file_browser
                {
                    browser.focused = false;
                }
            }
            BrowserAction::OpenDefault(path) => {
                let opened = crate::link_opener::open_file_in_default_app(&path);
                browser.set_notice(if opened {
                    "Opening in default app…".into()
                } else {
                    format!("Could not open {} in its default app", path.display())
                });
            }
            BrowserAction::Edit { path, editor } => {
                if let Err(error) = browser.start_editor(&path, editor) {
                    browser.set_notice(format!("{error:#}"));
                }
            }
        }
        Some(InputOutcome::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionRegistry;
    use crossterm::event::KeyEvent;

    fn event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn file_browser_keeps_the_draft_and_cycles_focus_back_to_the_tree() {
        let root = tempfile::tempdir().unwrap();
        let mut agent = crate::test_util::make_agent_view(None, root.path().to_str().unwrap());
        agent.prompt.set_text("draft");
        agent.open_file_browser();
        let registry = ActionRegistry::defaults();
        agent.handle_input(&Event::Paste("do not insert".into()), &registry);
        agent.handle_input(&event(KeyCode::Char('z')), &registry);
        assert_eq!(agent.prompt.text(), "draft");
        for _ in 0..2 {
            agent.handle_input(&event(KeyCode::Tab), &registry);
        }
        assert!(!agent.file_browser.as_ref().unwrap().focused);
        agent.handle_input(&event(KeyCode::Tab), &registry);
        assert!(agent.file_browser.as_ref().unwrap().focused);
        agent.handle_input(&event(KeyCode::Tab), &registry);
        assert!(agent.file_browser.as_ref().unwrap().focused);
        agent.handle_input(&event(KeyCode::Esc), &registry);
        assert!(!agent.file_browser_is_open());
        assert_eq!(agent.prompt.text(), "draft");
    }

    #[test]
    fn file_browser_yields_to_a_blocking_card_and_retargets_after_cd() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut agent = crate::test_util::make_agent_view(None, root.path().to_str().unwrap());
        agent.open_file_browser();
        agent.cancel_turn_view = Some(crate::views::modal::CancelTurnViewState {
            active_idx: 0,
            running_count: 1,
        });
        assert!(!agent.file_browser_can_draw());
        assert!(
            agent
                .handle_file_browser_input(&event(KeyCode::Esc))
                .is_none()
        );
        agent.cancel_turn_view = None;
        agent.session.cwd = other.path().to_path_buf();
        assert!(agent.poll_file_browser());
        assert!(agent.file_browser.is_none());
        agent.open_file_browser();
        assert_eq!(agent.file_browser.as_ref().unwrap().root(), other.path());
    }

    #[test]
    fn file_browser_replaces_the_conversation_area_and_restores_it_on_close() {
        let root = tempfile::tempdir().unwrap();
        let mut agent = crate::test_util::make_agent_view(None, root.path().to_str().unwrap());
        agent.prompt.set_text("draft remains");
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::system(
                "conversation marker",
            ));
        agent.open_file_browser();
        let text = render_text(&mut agent);
        assert!(text.contains("Files —"), "{text}");
        assert!(text.contains("Tree"), "{text}");
        assert!(text.contains("draft remains"), "{text}");
        assert!(!text.contains("conversation marker"), "{text}");
        agent.handle_file_browser_input(&event(KeyCode::Esc));
        assert!(render_text(&mut agent).contains("conversation marker"));
    }

    #[test]
    fn file_browser_layout_key_splits_the_area_and_keeps_the_conversation() {
        let root = tempfile::tempdir().unwrap();
        let mut agent = crate::test_util::make_agent_view(None, root.path().to_str().unwrap());
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::system(
                "conversation marker",
            ));
        agent.open_file_browser();
        let layout_key = Event::Key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        for expected in ["Top", "Left", "Right"] {
            agent.handle_file_browser_input(&layout_key);
            let text = render_text(&mut agent);
            assert!(text.contains(&format!("[Layout: {expected}]")), "{text}");
            assert!(text.contains("Files —"), "{text}");
            assert!(text.contains("conversation marker"), "{expected}: {text}");
        }
        agent.handle_file_browser_input(&layout_key);
        let text = render_text(&mut agent);
        assert!(text.contains("[Layout: Full]"), "{text}");
        assert!(!text.contains("conversation marker"), "{text}");
    }

    fn render_text(agent: &mut AgentView) -> String {
        let area = ratatui::layout::Rect::new(0, 0, 100, 30);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        agent.draw(
            area,
            &mut buf,
            &ActionRegistry::defaults(),
            &mut crate::scrollback::render::ScratchBuffer::new(),
            None,
            false,
            crate::app::agent_view::BannerSlotParams::none(),
            &crate::app::bundle::BundleState::default(),
            false,
            false,
            &mut Vec::new(),
            crate::app::agent_view::AppRenderParams::default(),
        );
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }
}
