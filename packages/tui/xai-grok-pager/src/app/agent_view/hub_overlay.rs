//! Controller for the hub browser overlay (`/hub open [name] [#channel]`).

use anyhow::Context;
use crossterm::event::{Event, KeyEventKind};
use xai_grok_tools::implementations::grok_build::agent_hub::{self, store};

use super::AgentView;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::key;
use crate::scrollback::block::RenderBlock;
use crate::views::hub_browser::{HubBrowserState, HubKeyOutcome, toggle_person_alerts};

impl AgentView {
    /// Open the browser on a hub: the named hub, or this session's hub when
    /// `hub` is `None`. This session's hub is the named hub it is in, or else
    /// the private hub of main and its subagents. On failure, say why in the
    /// scrollback instead.
    pub(crate) fn open_hub_browser(&mut self, hub: Option<&str>, channel: Option<&str>) {
        match self.load_hub_browser(hub, channel) {
            Ok(mut browser) => {
                browser.composer.set_history(self.hub_post_history.clone());
                browser.is_bot_thinking = self.is_bot_thinking_in(&browser.hub);
                browser.session_hub = self.private_hub_name(&browser.hubs_root());
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
        // A finished drag in the thread copies its text, like the scrollback.
        if let Some(text) = browser.copy_text.take() {
            self.copy_to_clipboard(&text);
        }
        let browser = self.hub_browser.as_mut()?;
        match outcome {
            HubKeyOutcome::Close => self.close_hub_browser(),
            HubKeyOutcome::Changed => {}
            HubKeyOutcome::Unchanged => return Some(InputOutcome::Unchanged),
            HubKeyOutcome::OpenLink(target) => {
                return Some(InputOutcome::Action(Action::OpenLink(target)));
            }
            HubKeyOutcome::GoToMember(handle) => return Some(self.go_to_hub_member(&handle)),
            HubKeyOutcome::StopMember(handle) => return Some(self.stop_hub_member(&handle)),
            HubKeyOutcome::CreateChannel { name, purpose } => {
                let result = browser.hub.create_channel(&name, &purpose);
                browser.finish_create_channel(result);
            }
            HubKeyOutcome::ToggleHere => {
                let result = toggle_person_alerts(&browser.hub);
                browser.finish_toggle_here(result);
            }
            HubKeyOutcome::SwitchHub(name) => self.switch_hub_browser(&name),
            HubKeyOutcome::Post { to, text } | HubKeyOutcome::Reply { to, text, .. }
                if is_bot_target(&to) =>
            {
                return Some(self.ask_hub_bot(&text));
            }
            HubKeyOutcome::Post { to, text } => {
                let result =
                    agent_hub::post_as_person(&browser.hub, browser.person_name(), &to, &text, &[]);
                finish_hub_post(browser, &to, result);
            }
            HubKeyOutcome::Reply { to, text, reply_to } => {
                let result = agent_hub::reply_as_person(
                    &browser.hub,
                    browser.person_name(),
                    &to,
                    &text,
                    &reply_to,
                );
                finish_hub_post(browser, &to, result);
            }
        }
        Some(InputOutcome::Changed)
    }

    /// Purpose: Write a question of the person to hub-bot and start the run.
    /// Input: The question text.
    /// Output: The action that runs the question, or a notice when it cannot run.
    /// Note: The next question resumes the last run, when this session made it.
    fn ask_hub_bot(&mut self, text: &str) -> InputOutcome {
        let session_id = self.session.session_id.as_ref().map(|s| s.0.to_string());
        let Some(browser) = self.hub_browser.as_mut() else {
            return InputOutcome::Changed;
        };
        let Some(session_id) = session_id else {
            browser.notice = Some("hub-bot needs a connected session.".to_string());
            return InputOutcome::Changed;
        };
        if browser.is_bot_thinking {
            browser.notice = Some("hub-bot still works on the last question.".to_string());
            return InputOutcome::Changed;
        }
        let to = format!("@{}", store::BOT_HANDLE);
        let question = match agent_hub::ask_bot(&browser.hub, browser.person_name(), text) {
            Ok(question) => question,
            Err(error) => {
                finish_hub_post(browser, &to, Err(error));
                return InputOutcome::Changed;
            }
        };
        finish_hub_post(browser, &to, Ok(question.id.clone()));
        browser.is_bot_thinking = true;
        let previous_child_id = browser
            .hub
            .bot_thread(&question.from)
            .filter(|thread| thread.session_id == session_id)
            .map(|thread| thread.child_id);
        let request = xai_grok_shell::extensions::hub_bot::AskRequest {
            session_id,
            child_id: uuid::Uuid::new_v4().to_string(),
            previous_child_id,
            hub: browser.hub.name().to_string(),
            person: question.from,
            question: text.to_string(),
        };
        let hub = browser.hub.clone();
        self.hub_bot_children
            .insert(request.child_id.clone(), hub.dir().to_path_buf());
        InputOutcome::Action(Action::AskHubBot { hub, request })
    }

    /// Close the browser and keep its post history for the next one.
    pub(crate) fn close_hub_browser(&mut self) {
        if let Some(browser) = self.hub_browser.take() {
            self.hub_post_history = browser.composer.history().to_vec();
        }
    }

    /// Purpose: Show another hub in the open browser (`H`).
    /// Input: The `Hub::name` of the hub.
    /// Output: None. On failure the browser stays on its hub with a notice.
    /// Note: This only views the hub. The session does not join it.
    fn switch_hub_browser(&mut self, name: &str) {
        let Some(browser) = self.hub_browser.as_mut() else {
            return;
        };
        let result =
            open_hub_by_name(&browser.hubs_root(), name).and_then(|hub| browser.switch_hub(hub));
        if let Err(error) = result {
            browser.notice = Some(format!("Could not show hub {name}: {error}"));
            return;
        }
        let hub = browser.hub.clone();
        let is_bot_thinking = self.is_bot_thinking_in(&hub);
        if let Some(browser) = self.hub_browser.as_mut() {
            browser.is_bot_thinking = is_bot_thinking;
        }
    }

    /// The `Hub::name` of the private hub of this session, when it exists.
    fn private_hub_name(&self, root: &std::path::Path) -> Option<String> {
        let session_id = self.session.session_id.as_ref()?;
        let hub = store::Hub::open_existing_session_hub(root, &session_id.0).ok()?;
        Some(hub.name().to_string())
    }

    /// True while a hub-bot question about `hub` runs.
    pub(crate) fn is_bot_thinking_in(&self, hub: &store::Hub) -> bool {
        self.hub_bot_children.values().any(|dir| dir == hub.dir())
    }

    /// Purpose: Show the session of a hub member.
    /// Input: The member handle.
    /// Output: The outcome. A subagent of this view opens full screen, this
    ///     session closes the hub, and another session goes to the app to focus.
    fn go_to_hub_member(&mut self, handle: &str) -> InputOutcome {
        let Some(session_id) = self.hub_member_session(handle) else {
            return InputOutcome::Changed;
        };
        if let Some(child) = self.subagent_views.get_mut(&session_id) {
            // The child can keep a browser of its own, which would cover it.
            child.hub_browser = None;
            self.hub_browser = None;
            self.open_subagent_fullscreen(session_id);
            return InputOutcome::Changed;
        }
        if self.is_own_session(&session_id) {
            self.hub_browser = None;
            return InputOutcome::Changed;
        }
        InputOutcome::Action(Action::FocusHubMember {
            session_id,
            handle: handle.to_string(),
        })
    }

    /// Purpose: Stop the running turn of a hub member.
    /// Input: The member handle.
    /// Output: A cancel for this session or a kill for one of its subagents.
    ///     Any other session goes to the app, which stops it when another tab
    ///     of this window runs it.
    fn stop_hub_member(&mut self, handle: &str) -> InputOutcome {
        let Some(session_id) = self.hub_member_session(handle) else {
            return InputOutcome::Changed;
        };
        if self.subagent_views.contains_key(&session_id) {
            return InputOutcome::Action(Action::KillSubagent(session_id));
        }
        if self.is_own_session(&session_id) {
            // The cancel can open a chooser for running subagents, and the
            // browser would cover it and take its keys.
            self.hub_browser = None;
            return InputOutcome::Action(Action::CancelTurn);
        }
        InputOutcome::Action(Action::StopHubMember {
            session_id,
            handle: handle.to_string(),
        })
    }

    /// The session id of a member, or a notice in the browser when it has none.
    fn hub_member_session(&mut self, handle: &str) -> Option<String> {
        let browser = self.hub_browser.as_mut()?;
        let session_id = browser
            .snapshot
            .meta
            .member(handle)
            .and_then(|member| member.session_id.clone());
        if session_id.is_none() {
            browser.notice = Some(format!("@{handle} has no session."));
        }
        session_id
    }

    fn is_own_session(&self, session_id: &str) -> bool {
        self.session
            .session_id
            .as_ref()
            .is_some_and(|sid| &*sid.0 == session_id)
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

/// Read the hub again after a post, then show the post result.
/// True when a post goes to hub-bot, which answers instead of a member.
fn is_bot_target(to: &str) -> bool {
    to.trim().trim_start_matches('@') == store::BOT_HANDLE
}

fn finish_hub_post(browser: &mut HubBrowserState, to: &str, result: anyhow::Result<String>) {
    // The post can reserve a new person handle, so read it before the notice.
    if let Err(error) = browser.reload() {
        tracing::warn!(%error, "hub browser: reload failed");
    }
    browser.finish_post(to, result);
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

/// Purpose: Open a hub for the hub switcher, as `/hub open` does.
/// Input: The hubs root and the `Hub::name` of a named hub or a session hub.
/// Output: The hub. The person becomes a member of a session hub that holds
///     messages, as in `open_hub_of_session`.
fn open_hub_by_name(root: &std::path::Path, name: &str) -> anyhow::Result<store::Hub> {
    let hub = store::Hub::open_by_name(root, name)?;
    if hub.meta()?.is_session_hub() && hub.has_messages() {
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

    #[test]
    fn a_post_to_hub_bot_writes_the_question_and_asks_the_bot() {
        let (_root, mut view, bus) = browser_with_agent();
        view.session.session_id = Some(agent_client_protocol::SessionId::new("root"));
        let browser = view.hub_browser.as_mut().unwrap();
        browser.open_dm_with(store::BOT_HANDLE);
        assert!(browser.is_bot_dm());
        view.handle_hub_browser_input(&key_event(KeyCode::Tab));
        view.handle_hub_browser_input(&Event::Paste("who works on login?".to_string()));
        let outcome = view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let Some(InputOutcome::Action(Action::AskHubBot { hub, request })) = outcome else {
            panic!("expected an ask, got {outcome:?}");
        };
        assert_eq!(hub.dir(), bus.hub().dir());
        assert_eq!(request.question, "who works on login?");
        assert_eq!(request.person, "me");
        assert_eq!(request.session_id, "root");
        assert_eq!(request.previous_child_id, None);
        assert!(view.hub_bot_children.contains_key(&request.child_id));
        let browser = view.hub_browser.as_ref().unwrap();
        assert!(browser.is_bot_thinking);
        assert!(browser.composer.is_empty());
        assert_eq!(browser.feed.last().unwrap().text, "who works on login?");
        let again = view.handle_hub_browser_input(&key_event(KeyCode::Tab));
        assert!(matches!(again, Some(InputOutcome::Changed)));
    }

    #[test]
    fn a_question_in_one_hub_does_not_mark_another_hub_thinking() {
        let (root, mut view, bus) = browser_with_agent();
        view.hub_bot_children
            .insert("child".to_string(), bus.hub().dir().to_path_buf());
        assert!(view.is_bot_thinking_in(&bus.hub()));
        let other = store::Hub::create(root.path(), "other").unwrap();
        assert!(!view.is_bot_thinking_in(&other));
        view.hub_bot_children.remove("child");
        assert!(!view.is_bot_thinking_in(&bus.hub()));
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn c_and_h_change_the_shown_hub_through_the_controller() {
        let (_root, mut view, bus) = browser_with_agent();
        view.handle_hub_browser_input(&key_event(KeyCode::Char('c')));
        view.handle_hub_browser_input(&Event::Paste("auth-fix".to_string()));
        view.handle_hub_browser_input(&key_event(KeyCode::Tab));
        view.handle_hub_browser_input(&Event::Paste("login work".to_string()));
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let meta = bus.hub().meta().unwrap();
        let channel = meta.channels.iter().find(|c| c.name == "auth-fix");
        assert_eq!(channel.map(|c| c.purpose.as_str()), Some("login work"));
        let browser = view.hub_browser.as_ref().unwrap();
        assert!(browser.channel_form.is_none());
        assert_eq!(
            browser.selected_row(),
            Some(crate::views::hub_browser::Row::Channel(
                "auth-fix".to_string()
            ))
        );

        assert!(meta.person_posts_alert_all);
        view.handle_hub_browser_input(&key_event(KeyCode::Char('h')));
        assert!(!bus.hub().meta().unwrap().person_posts_alert_all);
        let notice = view.hub_browser.as_ref().unwrap().notice.clone();
        assert!(notice.is_some_and(|n| n.starts_with("/hub here off")));
    }

    #[test]
    fn switch_hub_shows_another_hub_in_place_and_keeps_the_history() {
        let (root, mut view, bus) = browser_with_agent();
        store::Hub::create(root.path(), "other").unwrap();
        let browser = view.hub_browser.as_mut().unwrap();
        browser
            .composer
            .set_history(vec!["earlier post".to_string()]);
        view.switch_hub_browser("other");
        let browser = view.hub_browser.as_ref().unwrap();
        assert_eq!(browser.hub.name(), "other");
        assert_eq!(browser.person_name(), "me");
        assert_eq!(browser.composer.history(), ["earlier post".to_string()]);
        assert!(browser.is_person_view());

        view.switch_hub_browser("missing");
        let browser = view.hub_browser.as_ref().unwrap();
        assert_eq!(browser.hub.name(), "other", "a failed switch keeps the hub");
        assert!(browser.notice.as_ref().unwrap().contains("missing"));
        assert_eq!(
            bus.hub().name(),
            "team",
            "viewing a hub does not move the session"
        );
    }

    #[test]
    fn oversized_post_keeps_the_draft_until_a_successful_retry() {
        let (_root, mut view, bus) = browser_with_agent();
        view.handle_hub_browser_input(&key_event(KeyCode::Tab));
        let draft = format!("  {}  ", "x".repeat(MAX_MESSAGE_BYTES + 1));
        view.handle_hub_browser_input(&Event::Paste(draft.clone()));
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let browser = view.hub_browser.as_mut().unwrap();
        assert_eq!(browser.composer.text(), draft);
        assert!(browser.is_composing);
        assert_eq!(browser.post_target().as_deref(), Some("#general"));
        assert!(browser.notice.as_ref().unwrap().starts_with("Not posted:"));
        browser.composer.set_text("retry");
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        let browser = view.hub_browser.as_ref().unwrap();
        assert!(browser.composer.is_empty());
        assert!(browser.notice.as_ref().unwrap().starts_with("Posted to"));
        let messages = bus.hub().messages("#general", 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "retry");
    }

    #[test]
    fn post_to_a_gone_recipient_keeps_the_draft_and_destination() {
        let (_root, mut view, bus) = browser_with_agent();
        view.handle_hub_browser_input(&key_event(KeyCode::Down));
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
        assert_eq!(browser.composer.text(), "please check");
        assert_eq!(browser.post_target().as_deref(), Some("@worker"));
        assert!(browser.notice.as_ref().unwrap().contains("gone"));
        bus.register(MAIN_PEER_ID, "main", None);
        view.handle_hub_browser_input(&key_event(KeyCode::Enter));
        assert!(view.hub_browser.as_ref().unwrap().composer.is_empty());
        assert_eq!(bus.inbox(MAIN_PEER_ID).unwrap()[0].text, "please check");
    }

    #[test]
    fn member_actions_on_this_session_cancel_or_close_and_others_focus() {
        let (_root, mut view, bus) = browser_with_agent();
        view.session.session_id = Some(agent_client_protocol::SessionId::new("root"));
        assert!(matches!(
            view.stop_hub_member("worker"),
            InputOutcome::Action(Action::CancelTurn)
        ));
        assert!(matches!(
            view.go_to_hub_member("worker"),
            InputOutcome::Changed
        ));
        assert!(
            view.hub_browser.is_none(),
            "going to this session closes the hub"
        );
        view.session.session_id = Some(agent_client_protocol::SessionId::new("other"));
        view.hub_browser = Some(HubBrowserState::open(bus.hub(), "me", None).unwrap());
        match view.go_to_hub_member("worker") {
            InputOutcome::Action(Action::FocusHubMember { session_id, handle }) => {
                assert_eq!(session_id, "root");
                assert_eq!(handle, "worker");
            }
            other => panic!("expected a focus action, got {other:?}"),
        }
        match view.stop_hub_member("worker") {
            InputOutcome::Action(Action::StopHubMember { session_id, handle }) => {
                assert_eq!(session_id, "root");
                assert_eq!(handle, "worker");
            }
            other => panic!("expected a stop action for the app, got {other:?}"),
        }
    }
}
