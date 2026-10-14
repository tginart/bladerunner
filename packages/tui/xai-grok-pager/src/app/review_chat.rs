use super::{
    actions::Effect,
    app_view::{ActiveView, AppView},
};
use crate::acp::meta::NotificationMeta;
use agent_client_protocol as acp;
use xai_grok_shell::extensions::{
    notification::{SessionNotification, SessionUpdate as XaiSessionUpdate},
    review_chat::{AGENT_NAME, AskRequest},
};

pub(super) fn dispatch_ask(app: &mut AppView, req: AskRequest) -> Vec<Effect> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get(&agent_id) else {
        return vec![];
    };
    if app.reconnect_pending
        || agent.session.session_id.as_ref().map(|s| s.0.as_ref()) != Some(req.session_id.as_str())
    {
        if let Some(chat) = &agent.review_chat {
            chat.borrow_mut()
                .refuse_question(&req.child_id, "Reconnecting. Try again when connected.");
        }
        return vec![];
    }
    vec![Effect::AskReviewChat {
        agent_id,
        request: req,
    }]
}

/// Consume review-child traffic before ordinary subagent routing so neither
/// the internal diff prompt nor the answer appears in the main transcript.
pub(super) fn route_update(
    app: &mut AppView,
    sid: &acp::SessionId,
    update: &acp::SessionUpdate,
    meta: &NotificationMeta,
) -> Option<bool> {
    for agent in app.agents.values_mut() {
        let Some(shared) = &agent.review_chat else {
            continue;
        };
        let mut chat = shared.borrow_mut();
        if chat.current_child_id.as_deref() == Some(sid.0.as_ref()) {
            chat.receive_update(update.clone(), meta);
            return Some(true);
        }
    }
    None
}

pub(super) fn route_lifecycle(app: &mut AppView, notif: &SessionNotification) -> Option<bool> {
    if let XaiSessionUpdate::SubagentSpawned {
        subagent_type,
        parent_session_id,
        child_session_id,
        model,
        ..
    } = &notif.update
        && subagent_type == AGENT_NAME
    {
        for agent in app.agents.values_mut() {
            if let Some(shared) = &agent.review_chat {
                let mut chat = shared.borrow_mut();
                if chat.parent_session_id == *parent_session_id
                    && chat.current_child_id.as_deref() == Some(child_session_id.as_str())
                {
                    chat.model_id = model.clone();
                    chat.sync_model_name(&agent.session.models);
                    break;
                }
            }
        }
        return Some(true);
    }
    for agent in app.agents.values_mut() {
        let Some(shared) = &agent.review_chat else {
            continue;
        };
        let chat = shared.borrow();
        if chat.current_child_id.as_deref() == Some(notif.session_id.0.as_ref()) {
            return Some(true);
        }
        if let XaiSessionUpdate::SubagentFinished {
            child_session_id, ..
        } = &notif.update
            && chat.current_child_id.as_deref() == Some(child_session_id.as_str())
        {
            return Some(true);
        }
    }
    None
}
