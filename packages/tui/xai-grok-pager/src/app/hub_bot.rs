//! Pager side of hub-bot: send the person's question, write the answer into
//! the hub DM, and keep the bot child's traffic out of the transcript.
use super::{
    actions::Effect,
    agent::AgentId,
    app_view::{ActiveView, AppView},
};
use agent_client_protocol as acp;
use xai_grok_shell::extensions::{
    hub_bot::{AGENT_NAME, AskRequest, AskResponse},
    notification::{SessionNotification, SessionUpdate as XaiSessionUpdate},
};
use xai_grok_tools::implementations::grok_build::agent_hub::{self, store};

/// Purpose: Start one hub-bot question for the active agent view.
/// Input: The app, the hub, and the request.
/// Output: The effect that sends the request, or none when the session is
///     not connected. Then the DM gets an answer that says so.
pub(super) fn dispatch_ask(app: &mut AppView, hub: store::Hub, request: AskRequest) -> Vec<Effect> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let is_connected = app.agents.get(&agent_id).is_some_and(|agent| {
        agent.session.session_id.as_ref().map(|s| s.0.as_ref()) == Some(request.session_id.as_str())
    });
    if app.reconnect_pending || !is_connected {
        let refusal = Err("the session is not connected. Try again when it is.".to_string());
        return finish_answer(app, agent_id, &hub, &request, refusal);
    }
    vec![Effect::AskHubBot {
        agent_id,
        hub,
        request,
    }]
}

/// Purpose: Write the answer of hub-bot into its DM with the person.
/// Input: The app, the agent view, the hub, the request, and the response.
/// Output: No effects. The open browser stops "thinking" and reads the hub.
/// Note: Only a successful run becomes the conversation that the next
///     question resumes.
pub(super) fn finish_answer(
    app: &mut AppView,
    agent_id: AgentId,
    hub: &store::Hub,
    request: &AskRequest,
    response: Result<AskResponse, String>,
) -> Vec<Effect> {
    if let Ok(answer) = &response
        && answer.success
    {
        let thread = store::BotThread {
            session_id: request.session_id.clone(),
            child_id: request.child_id.clone(),
        };
        if let Err(error) = hub.save_bot_thread(&request.person, &thread) {
            tracing::warn!(%error, "hub-bot: could not save the thread");
        }
    }
    let text = answer_text(response);
    if let Err(error) = agent_hub::answer_as_bot(hub, &request.person, &text) {
        tracing::warn!(%error, "hub-bot: could not write the answer");
    }
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    agent.hub_bot_children.remove(&request.child_id);
    let Some(is_thinking) = agent
        .hub_browser
        .as_ref()
        .map(|browser| agent.is_bot_thinking_in(&browser.hub))
    else {
        return vec![];
    };
    let browser = agent.hub_browser.as_mut().expect("checked above");
    // The open browser can show another hub, which has its own questions.
    browser.is_bot_thinking = is_thinking;
    if browser.hub.dir() == hub.dir()
        && let Err(error) = browser.reload()
    {
        tracing::warn!(%error, "hub browser: reload failed");
    }
    vec![]
}

/// The text that hub-bot posts for a response.
fn answer_text(response: Result<AskResponse, String>) -> String {
    let error = match response {
        Ok(answer) if answer.success && !answer.output.trim().is_empty() => {
            return answer.output;
        }
        Ok(answer) if answer.success => "the answer was empty".to_string(),
        Ok(answer) => answer.error.unwrap_or_else(|| "the run failed".to_string()),
        Err(error) => error,
    };
    format!("I could not answer: {error}")
}

/// True when `child_id` is a hub-bot run of some agent view.
fn is_bot_child(app: &AppView, child_id: &str) -> bool {
    app.agents
        .values()
        .any(|agent| agent.hub_bot_children.contains_key(child_id))
}

/// Drop the streamed updates of a hub-bot run. They are not transcript content.
pub(super) fn route_update(app: &AppView, sid: &acp::SessionId) -> Option<bool> {
    is_bot_child(app, sid.0.as_ref()).then_some(false)
}

/// Drop the lifecycle events of hub-bot runs, so no subagent view opens.
pub(super) fn route_lifecycle(app: &AppView, notif: &SessionNotification) -> Option<bool> {
    let is_bot_event = match &notif.update {
        XaiSessionUpdate::SubagentSpawned { subagent_type, .. } => subagent_type == AGENT_NAME,
        XaiSessionUpdate::SubagentFinished {
            child_session_id, ..
        } => is_bot_child(app, child_session_id),
        _ => is_bot_child(app, notif.session_id.0.as_ref()),
    };
    is_bot_event.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(success: bool, output: &str, error: Option<&str>) -> AskResponse {
        AskResponse {
            child_id: "c".to_string(),
            success,
            output: output.to_string(),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn answers_say_why_a_question_failed() {
        assert_eq!(answer_text(Ok(response(true, "@front", None))), "@front");
        assert_eq!(
            answer_text(Ok(response(true, " ", None))),
            "I could not answer: the answer was empty"
        );
        assert_eq!(
            answer_text(Ok(response(false, "", Some("Question stopped.")))),
            "I could not answer: Question stopped."
        );
        assert_eq!(
            answer_text(Err("offline".to_string())),
            "I could not answer: offline"
        );
    }
}
