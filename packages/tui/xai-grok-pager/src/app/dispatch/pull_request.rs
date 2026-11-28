//! Connect the PR corner card, minimized chip and fullscreen review workspace.
//! Each session owns its selected PR. A fetch can complete while the card is closed.

use crate::app::{
    actions::Effect,
    agent_view::AgentView,
    app_view::{ActiveView, AppView},
};
use crate::views::{
    modal::ActiveModal,
    pull_request::{
        Viewer,
        summary::{Control, Display},
    },
};

pub(super) fn open(app: &mut AppView, target: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if target.is_empty()
        && let Some(viewer) = &mut agent.pull_request
    {
        viewer.summary.display = Display::Card;
        viewer.summary.clear_hits();
        return vec![];
    }
    vec![fetch(agent, target)]
}

/// Purpose: Activate a PR control from either the mouse or a slash command.
/// Input: The active session and user action. Output: UI state and optional fetch.
pub(super) fn control(app: &mut AppView, control: Control) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if control == Control::Refresh {
        let target = agent
            .pull_request
            .as_ref()
            .map(|pr| pr.target.clone())
            .unwrap_or_default();
        return vec![fetch(agent, target)];
    }
    let mut effects = vec![];
    if agent.pull_request.is_none() && matches!(control, Control::Expand | Control::Show) {
        effects.push(fetch(agent, String::new()));
    }
    let Some(viewer) = &mut agent.pull_request else {
        return effects;
    };
    viewer.summary.clear_hits();
    viewer.summary.display = match control {
        Control::Expand => {
            viewer.enter_fullscreen();
            agent.active_modal = Some(ActiveModal::PullRequest);
            Display::Card
        }
        Control::Minimize => Display::Chip,
        Control::Close => Display::Closed,
        Control::Show => Display::Card,
        Control::Refresh => unreachable!("refresh returned above"),
    };
    if control != Control::Expand && matches!(agent.active_modal, Some(ActiveModal::PullRequest)) {
        agent.active_modal = None;
    }
    effects
}

fn fetch(agent: &mut AgentView, target: String) -> Effect {
    let mut viewer = Viewer::loading();
    viewer.target = target.clone();
    let request_id = viewer.request_id;
    agent.pull_request = Some(Box::new(viewer));
    Effect::FetchPullRequest {
        agent_id: agent.session.id,
        request_id,
        cwd: agent.session.cwd.clone(),
        target,
    }
}
