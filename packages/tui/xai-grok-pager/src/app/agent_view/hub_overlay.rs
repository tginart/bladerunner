//! Controller for the hub browser overlay (`/hub open [name] [#channel]`).

use anyhow::Context;
use crossterm::event::{Event, KeyEventKind};
use xai_grok_tools::implementations::grok_build::agent_hub::{self, store};

use super::AgentView;
use crate::app::app_view::InputOutcome;
use crate::key;
use crate::scrollback::block::RenderBlock;
use crate::views::hub_browser::{HubBrowserState, HubKeyOutcome};

impl AgentView {
    /// Open the browser on a hub: the named hub, or this session's hub when
    /// `hub` is `None`. This session's hub is the named hub it is in, or else
    /// the private hub of main and its subagents. On failure, say why in the
    /// scrollback instead.
    pub(crate) fn open_hub_browser(&mut self, hub: Option<&str>, channel: Option<&str>) {
        match self.load_hub_browser(hub, channel) {
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
        let outcome = match ev {
            Event::Key(key) if key.kind == KeyEventKind::Release => HubKeyOutcome::Changed,
            Event::Key(key) if key!('q', CONTROL).matches(key) => {
                return Some(InputOutcome::Unchanged);
            }
            Event::Key(key) => browser.handle_key(key),
            Event::Paste(text) => {
                browser.handle_paste(text);
                HubKeyOutcome::Changed
            }
            Event::Mouse(mouse) => browser.handle_mouse(mouse.kind, mouse.column, mouse.row),
            _ => HubKeyOutcome::Changed,
        };
        match outcome {
            HubKeyOutcome::Close => self.hub_browser = None,
            HubKeyOutcome::Changed => {}
            HubKeyOutcome::Post { to, text } => {
                let result =
                    agent_hub::post_as_person(&browser.hub, browser.person_name(), &to, &text, &[]);
                // The post can reserve a new person handle, so read it before the notice.
                if let Err(error) = browser.reload() {
                    tracing::warn!(%error, "hub browser: reload failed");
                }
                browser.finish_post(&to, result);
            }
        }
        Some(InputOutcome::Changed)
    }

    fn load_hub_browser(
        &self,
        hub: Option<&str>,
        channel: Option<&str>,
    ) -> anyhow::Result<HubBrowserState> {
        let root = xai_grok_shell::session::hub::hubs_root();
        let hub = match hub {
            Some(name) => open_named_hub(&root, name)?,
            None => self.open_session_hub(&root)?,
        };
        HubBrowserState::open(hub, &store::person_handle(), channel)
    }

    fn open_session_hub(&self, root: &std::path::Path) -> anyhow::Result<store::Hub> {
        let session_id = self
            .session
            .session_id
            .as_ref()
            .context("the session has not started yet")?;
        open_hub_of_session(root, &session_id.0)
    }
}

/// Purpose: Open the hub of a session for the browser.
/// Input: The hubs root and the session id.
/// Output: The named hub where the session is a member, or else the private
///     session hub of main and its subagents.
/// Note: The person becomes a member of a session hub that holds messages.
///     The browser saves the read positions of members only, so without this
///     the unread count of the status-bar label never goes down.
fn open_hub_of_session(root: &std::path::Path, session_id: &str) -> anyhow::Result<store::Hub> {
    if let Some((name, _handle)) = store::find_membership(root, session_id) {
        return open_named_hub(root, &name);
    }
    let hub = store::Hub::open_existing_session_hub(root, session_id).context(
        "this session has no hub yet. Use /hub open <name>, /hub join <name>, or /hub new <name>.",
    )?;
    // An empty session hub stays free of a person, so the prune can remove it.
    if hub.has_messages() {
        agent_hub::ensure_person(&hub, &store::person_handle())?;
    }
    Ok(hub)
}

/// Open a hub by name, with a suggestion when the name does not exist.
fn open_named_hub(root: &std::path::Path, name: &str) -> anyhow::Result<store::Hub> {
    if let Ok(hub) = store::Hub::open(root, name) {
        return Ok(hub);
    }
    match store::suggest_hub_name(root, name) {
        Some(suggestion) => anyhow::bail!("no hub named {name}. Did you mean {suggestion}?"),
        None => anyhow::bail!("no hub named {name}. Use /hub list to see the hubs."),
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
        let mut view = crate::test_util::make_agent_view(None, "/tmp");
        view.hub_browser = Some(HubBrowserState::open(bus.hub(), "me", None).unwrap());
        (root, view, bus)
    }

    #[test]
    fn session_hub_opens_when_the_session_is_in_no_named_hub() {
        let root = tempfile::tempdir().unwrap();
        let error = open_hub_of_session(root.path(), "root").unwrap_err();
        assert!(error.to_string().contains("no hub yet"), "{error}");
        let bus = AgentBus::new(root.path().to_path_buf(), "root").unwrap();
        bus.register(MAIN_PEER_ID, "main", None);
        let hub = open_hub_of_session(root.path(), "root").unwrap();
        let meta = hub.meta().unwrap();
        assert!(meta.is_session_hub());
        assert!(
            meta.members
                .iter()
                .all(|m| m.kind != agent_hub::MemberKind::Person)
        );
        bus.join("team", Some("worker"), "unused", true).unwrap();
        assert_eq!(
            open_hub_of_session(root.path(), "root").unwrap().name(),
            "team"
        );
    }

    #[test]
    fn reading_the_session_hub_clears_its_unread_label() {
        let root = tempfile::tempdir().unwrap();
        let bus = AgentBus::new(root.path().to_path_buf(), "root").unwrap();
        bus.register(MAIN_PEER_ID, "main", None);
        bus.send(MAIN_PEER_ID, "#general", "status", None, &[])
            .into_result()
            .unwrap();
        let hub = open_hub_of_session(root.path(), "root").unwrap();
        let person = store::person_handle();
        let browser = HubBrowserState::open(hub.clone(), &person, Some("#general")).unwrap();
        assert!(browser.feed.iter().any(|m| m.text == "status"));
        let handle = agent_hub::person_handle_in(&hub.meta().unwrap(), &person);
        let read = hub.cursors(&handle).unwrap().read;
        assert_eq!(read.get("#general").copied(), Some(1));
    }

    #[test]
    fn wheel_scroll_goes_to_the_open_hub_thread() {
        let (_root, mut view, bus) = browser_with_agent();
        let hub = bus.hub();
        for i in 0..80 {
            agent_hub::post_as_person(&hub, "me", "#general", &format!("line {i}"), &[]).unwrap();
        }
        let area = ratatui::layout::Rect::new(0, 0, 100, 24);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        let browser = view.hub_browser.as_mut().unwrap();
        browser.reload().unwrap();
        crate::views::hub_browser::render_hub_browser(&mut buf, area, browser);

        view.handle_scroll(-4, 70, 12);
        assert_eq!(view.hub_browser.as_ref().unwrap().feed_scroll, 4);
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
        assert_eq!(
            view.hub_browser.as_ref().unwrap().post_target().as_deref(),
            Some("@worker")
        );
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
