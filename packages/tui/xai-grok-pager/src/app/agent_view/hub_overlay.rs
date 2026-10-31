//! Controller for the hub browser overlay (`/hub open`): open, input, refresh.

use anyhow::Context;
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use xai_grok_tools::implementations::grok_build::agent_hub::{self, store};

use super::AgentView;
use crate::app::app_view::InputOutcome;
use crate::key;
use crate::scrollback::block::RenderBlock;
use crate::views::hub_browser::{HubBrowserState, HubKeyOutcome};
use crate::views::modal_window::{ModalWindowOutcome, handle_modal_mouse};

/// Feed lines that one mouse wheel step scrolls.
const WHEEL_LINES: usize = 3;

impl AgentView {
    /// Open the browser on this session's hub. When the session is in no
    /// named hub, say so in the scrollback instead.
    pub(crate) fn open_hub_browser(&mut self, channel: Option<&str>) {
        match self.load_hub_browser(channel) {
            Ok(browser) => {
                self.hub_browser = Some(browser);
                self.show_goal_detail = false;
                self.show_workflows = false;
            }
            Err(error) => {
                let message = format!("/hub open: {error}");
                self.scrollback.push_block(RenderBlock::system(message));
            }
        }
    }

    /// Read the open browser's hub again. Returns false when no browser is open.
    pub(crate) fn refresh_hub_browser(&mut self) -> bool {
        let Some(browser) = self.hub_browser.as_mut() else {
            return false;
        };
        if let Err(error) = browser.reload() {
            tracing::warn!(%error, "hub browser: reload failed");
        }
        true
    }

    pub(super) fn handle_hub_browser_input(&mut self, ev: &Event) -> Option<InputOutcome> {
        let browser = self.hub_browser.as_mut()?;
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Release => {}
            Event::Key(key) if key!('q', CONTROL).matches(key) => {
                return Some(InputOutcome::Unchanged);
            }
            Event::Key(key) => match browser.handle_key(key) {
                HubKeyOutcome::Close => self.hub_browser = None,
                HubKeyOutcome::Changed => {}
                HubKeyOutcome::Post { to, text } => {
                    let result =
                        agent_hub::post_as_person(&browser.hub, &browser.person, &to, &text, &[]);
                    browser.finish_post(&to, result);
                    if let Err(error) = browser.reload() {
                        tracing::warn!(%error, "hub browser: reload failed");
                    }
                }
            },
            Event::Paste(text) => browser.handle_paste(text),
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => browser.feed_scroll += WHEEL_LINES,
                    MouseEventKind::ScrollDown => {
                        browser.feed_scroll = browser.feed_scroll.saturating_sub(WHEEL_LINES);
                    }
                    _ => {}
                }
                let outcome =
                    handle_modal_mouse(&mut browser.window, mouse.kind, mouse.column, mouse.row);
                if outcome == ModalWindowOutcome::CloseRequested {
                    self.hub_browser = None;
                }
            }
            _ => {}
        }
        Some(InputOutcome::Changed)
    }

    fn load_hub_browser(&self, channel: Option<&str>) -> anyhow::Result<HubBrowserState> {
        let session_id = self
            .session
            .session_id
            .as_ref()
            .context("the session has not started yet")?;
        let root = xai_grok_shell::session::hub::hubs_root();
        let (name, _handle) = store::find_membership(&root, &session_id.0)
            .context("this session is in no hub. Use /hub join <name> or /hub new <name>.")?;
        let hub = store::Hub::open(&root, &name)?;
        let person = agent_hub::ensure_person(&hub, &store::person_handle())?;
        HubBrowserState::open(hub, person, channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hub::{AgentBus, MAIN_PEER_ID, MAX_MESSAGE_BYTES};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn browser_with_agent() -> (tempfile::TempDir, AgentView, AgentBus) {
        let root = tempfile::tempdir().unwrap();
        let bus = AgentBus::new(root.path().to_path_buf(), "root").unwrap();
        bus.register(MAIN_PEER_ID, "main", None);
        bus.join("team", Some("worker"), "unused", true).unwrap();
        let person = agent_hub::ensure_person(&bus.hub(), "me").unwrap();
        let mut view = crate::test_util::make_agent_view(None, "/tmp");
        view.hub_browser = Some(HubBrowserState::open(bus.hub(), person, None).unwrap());
        (root, view, bus)
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn oversized_post_keeps_the_draft_until_a_successful_retry() {
        let (_root, mut view, bus) = browser_with_agent();
        view.handle_hub_browser_input(&key_event(KeyCode::Tab));
        let draft = format!("  {}  ", "x".repeat(MAX_MESSAGE_BYTES + 1));
        view.handle_hub_browser_input(&Event::Paste(draft.clone()));
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let browser = view.hub_browser.as_mut().unwrap();
        assert_eq!(browser.draft, draft);
        assert!(browser.is_composing);
        assert_eq!(browser.post_target().as_deref(), Some("#general"));
        assert!(browser.notice.as_ref().unwrap().starts_with("Not posted:"));
        browser.draft = "retry".to_string();
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let browser = view.hub_browser.as_ref().unwrap();
        assert!(browser.draft.is_empty());
        assert!(browser.notice.as_ref().unwrap().starts_with("Posted to"));
        let messages = bus.hub().messages("#general", 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "retry");
    }

    #[test]
    fn post_to_a_gone_recipient_keeps_the_draft_and_destination() {
        let (_root, mut view, bus) = browser_with_agent();
        view.handle_hub_browser_input(&key_event(KeyCode::Down));
        view.handle_hub_browser_input(&key_event(KeyCode::Tab));
        view.handle_hub_browser_input(&Event::Paste("please check".to_string()));
        bus.mark_gone(MAIN_PEER_ID);
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let browser = view.hub_browser.as_ref().unwrap();
        assert_eq!(browser.draft, "please check");
        assert_eq!(browser.post_target().as_deref(), Some("@worker"));
        assert!(browser.notice.as_ref().unwrap().contains("gone"));
        bus.register(MAIN_PEER_ID, "main", None);
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        assert!(view.hub_browser.as_ref().unwrap().draft.is_empty());
        assert_eq!(bus.inbox(MAIN_PEER_ID).unwrap()[0].text, "please check");
    }
}
