//! Hub presence of a session: what the agent does now, for the hub browser.
//!
//! The bus writes `presence/<handle>.json` in the hub. This module calls it
//! at turn start and end, and around prompts that wait for a person.

use xai_grok_tools::implementations::grok_build::agent_hub::InputGuard;

use super::*;

impl SessionActor {
    /// Purpose: Report that a turn started.
    /// Input: The turn prompt blocks, and whether a wake or a drain started it.
    /// Output: None. A person or parent prompt becomes the task of the member.
    pub(super) fn report_hub_turn_started(
        &self,
        prompt_blocks: &[acp::ContentBlock],
        is_synthetic: bool,
    ) {
        let Some((bus, peer)) = self.hub_peer() else {
            return;
        };
        let task = (!is_synthetic).then(|| prompt_text(prompt_blocks));
        bus.turn_started(peer, task.as_deref());
    }

    pub(super) fn report_hub_turn_ended(&self) {
        if let Some((bus, peer)) = self.hub_peer() {
            bus.turn_ended(peer);
        }
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
