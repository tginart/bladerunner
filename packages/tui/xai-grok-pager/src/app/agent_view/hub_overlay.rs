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
                    browser.notice = Some(match result {
                        Ok(_) => format!("Posted to {to} as @{}.", browser.person),
                        Err(error) => format!("Not posted: {error}"),
                    });
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
        HubBrowserState::open(hub, store::person_handle(), channel)
    }
}
