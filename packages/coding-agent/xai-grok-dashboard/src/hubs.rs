//! Agent hubs on the dashboard: list hubs, show a feed, and post as a person.
//!
//! Hubs live under `<grok home>/hubs` (see `agent_hub::store`). Reads never
//! move a member's cursor. A post is the only write: it appends one message
//! as the person, exactly like `/hub post`.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use xai_grok_tools::implementations::grok_build::agent_hub::{self, MemberKind, store};

/// Messages that one hub page shows.
pub const FEED_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubSummary {
    pub name: String,
    pub active_members: usize,
    pub channels: usize,
    pub tokens: u64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedMessage {
    /// `#channel` or `DM @a/@b`.
    pub place: String,
    pub from: String,
    pub is_person: bool,
    pub text: String,
    pub ts_unix_ms: u64,
    pub images: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubPage {
    pub meta: store::HubMeta,
    /// Selected channel, or `None` for all channels and DMs.
    pub channel: Option<String>,
    pub messages: Vec<FeedMessage>,
    pub tokens: u64,
}

pub fn hubs_root(grok_home: &Path) -> std::path::PathBuf {
    grok_home.join(store::HUBS_DIR)
}

/// Visible hubs, archived ones included, sorted by name.
pub fn list(grok_home: &Path) -> Result<Vec<HubSummary>> {
    let hubs = store::list_hubs(&hubs_root(grok_home), true)?;
    Ok(hubs
        .into_iter()
        .map(|meta| HubSummary {
            active_members: meta.members.iter().filter(|m| m.is_live()).count(),
            channels: meta.channels.len(),
            tokens: meta.tokens.values().sum(),
            archived: meta.archived,
            name: meta.name,
        })
        .collect())
}

/// Purpose: Load one hub page.
/// Input: The grok home, the hub name, and an optional channel filter.
/// Output: The hub metadata and its latest messages, oldest first.
pub fn page(grok_home: &Path, name: &str, channel: Option<&str>) -> Result<HubPage> {
    let hub = store::Hub::open(&hubs_root(grok_home), name)?;
    let mut meta = hub.meta()?;
    // The page shows `active`, so resolve process liveness here on the server.
    for member in &mut meta.members {
        member.active = member.is_live();
    }
    let messages = store::recent_messages(&hub, channel, FEED_LIMIT)?
        .into_iter()
        .map(|m| FeedMessage {
            place: match store::split_dm_conv(&m.conv) {
                Some((a, b)) => format!("DM @{a}/@{b}"),
                None => m.conv.clone(),
            },
            is_person: meta
                .member(&m.from)
                .is_some_and(|member| member.kind == MemberKind::Person),
            from: m.from,
            text: m.text,
            ts_unix_ms: m.ts_unix_ms,
            images: m.images.len(),
        })
        .collect();
    Ok(HubPage {
        tokens: meta.tokens.values().sum(),
        channel: channel.map(|c| c.trim_start_matches('#').to_string()),
        meta,
        messages,
    })
}

/// Post `text` to `#channel` or `@handle` as the person at this machine.
pub fn post(grok_home: &Path, name: &str, to: &str, text: &str) -> Result<String> {
    let hub = store::Hub::open(&hubs_root(grok_home), name)?;
    agent_hub::post_as_person(&hub, &store::person_handle(), to, text, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_page_and_post() {
        let home = tempfile::tempdir().unwrap();
        assert!(list(home.path()).unwrap().is_empty());
        store::Hub::create(&hubs_root(home.path()), "api").unwrap();
        let hubs = list(home.path()).unwrap();
        assert_eq!(hubs[0].name, "api");
        assert_eq!(hubs[0].channels, 1);

        post(home.path(), "api", "#general", "ship it").unwrap();
        let page = page(home.path(), "api", None).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].place, "#general");
        assert!(page.messages[0].is_person);
        assert!(post(home.path(), "api", "#nope", "x").is_err());
        assert!(super::page(home.path(), "missing", None).is_err());
    }
}
