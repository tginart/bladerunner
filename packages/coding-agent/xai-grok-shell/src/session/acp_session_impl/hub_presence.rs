//! Hub presence of a session: what the agent does now, for the hub browser.
//!
//! The bus writes `presence/<handle>.json` in the hub. This module calls it
//! at turn start and end, and around prompts that wait for a person.

use xai_grok_tools::implementations::grok_build::agent_hub::{InputGuard, TurnGuard};

use super::*;

impl SessionActor {
    /// Purpose: Report that a turn started, until the turn ends.
    /// Input: The turn prompt blocks, and whether a wake or a drain started it.
    /// Output: A guard that the turn keeps, or None when the session is in no
    ///     hub. A person or parent prompt becomes the task of the member.
    /// Note: The member shows `idle` when the guard drops, on every exit path
    ///     of the turn: builtin commands and errors too.
    pub(super) fn hub_turn_guard(
        &self,
        prompt_blocks: &[acp::ContentBlock],
        is_synthetic: bool,
    ) -> Option<TurnGuard> {
        let (bus, peer) = self.hub_peer()?;
        let task = (!is_synthetic).then(|| prompt_text(prompt_blocks));
        Some(bus.turn_guard(peer, task.as_deref()))
    }

    /// A guard that shows the member as `needs_input` while a prompt for a
    /// person is open, or None when the session is in no hub.
    pub(crate) fn hub_input_guard(&self) -> Option<InputGuard> {
        let (bus, peer) = self.hub_peer()?;
        Some(bus.input_guard(peer))
    }

    /// The hub bus of this session and its peer id on the bus.
    fn hub_peer(&self) -> Option<(&xai_grok_tools::implementations::grok_build::AgentBus, &str)> {
        let bus = self.tool_context.agent_bus.as_ref()?;
        let peer = self
            .tool_context
            .agent_self_id
            .as_deref()
            .unwrap_or(xai_grok_tools::implementations::grok_build::MAIN_PEER_ID);
        Some((bus.bus(), peer))
    }
}

/// The text blocks of a prompt, joined with newlines.
fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let acp::ContentBlock::Text(block) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block.text);
        }
    }
    text
}
