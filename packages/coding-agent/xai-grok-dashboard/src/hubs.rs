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
    pub id: String,
    /// `#channel` or `DM @a/@b`.
    pub place: String,
    pub from: String,
    pub is_person: bool,
    pub text: String,
    pub ts_unix_ms: u64,
    pub images: usize,
    pub pinned: bool,
}

/// One pinned channel message, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedFeedMessage {
    pub pinned_by: String,
    pub message: FeedMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubPage {
    pub meta: store::HubMeta,
    /// Selected channel, or `None` for all channels and DMs.
    pub channel: Option<String>,
    pub messages: Vec<FeedMessage>,
    /// Pins of the selected channel, or of every channel. Read only.
    pub pinned: Vec<PinnedFeedMessage>,
    pub tokens: u64,
}

pub fn hubs_root(grok_home: &Path) -> std::path::PathBuf {
    grok_home.join(store::HUBS_DIR)
}

/// Visible hubs, archived ones included, sorted by name.
pub fn list(grok_home: &Path) -> Result<Vec<HubSummary>> {
    let root = hubs_root(grok_home);
    let mut out = Vec::new();
    for meta in store::list_hubs(&root, true)? {
        let tokens = store::Hub::open(&root, &meta.name)?.tokens(&meta)?;
        out.push(HubSummary {
            active_members: meta.members.iter().filter(|m| m.is_live()).count(),
            channels: meta.channels.len(),
            tokens: tokens.values().sum(),
            archived: meta.archived,
            name: meta.name,
        });
    }
    Ok(out)
}

/// Purpose: Load one hub page.
/// Input: The grok home, the hub name, and an optional channel filter.
/// Output: The hub metadata, its latest messages oldest first, and the
///     pinned messages of the selected channel or of every channel.
pub fn page(grok_home: &Path, name: &str, channel: Option<&str>) -> Result<HubPage> {
    let hub = store::Hub::open(&hubs_root(grok_home), name)?;
    let mut meta = hub.meta()?;
    // The page shows `active`, so resolve process liveness here on the server.
    for member in &mut meta.members {
        member.active = member.is_live();
    }
    let messages = store::recent_messages(&hub, channel, FEED_LIMIT)?
        .into_iter()
        .map(|m| feed_message(&meta, m))
        .collect();
    let mut pinned = Vec::new();
    for listed in &meta.channels {
        let is_shown = channel.is_none_or(|c| c.trim_start_matches('#') == listed.name);
        if !is_shown || listed.pins.is_empty() {
            continue;
        }
        for (pin, message) in hub.channel_pins(&listed.name)? {
            pinned.push(PinnedFeedMessage {
                pinned_by: pin.pinned_by,
                message: feed_message(&meta, message),
            });
        }
    }
    Ok(HubPage {
        tokens: hub.tokens(&meta)?.values().sum(),
        channel: channel.map(|c| c.trim_start_matches('#').to_string()),
        meta,
        messages,
        pinned,
    })
}

fn feed_message(meta: &store::HubMeta, m: store::Message) -> FeedMessage {
    let pinned = m
        .conv
        .strip_prefix('#')
        .and_then(|name| meta.channel(name))
        .is_some_and(|c| c.is_pinned(&m.id));
    FeedMessage {
        place: match store::split_dm_conv(&m.conv) {
            Some((a, b)) => format!("DM @{a}/@{b}"),
            None => m.conv.clone(),
        },
        is_person: meta
            .member(&m.from)
            .is_some_and(|member| member.kind == MemberKind::Person),
        id: m.id,
        from: m.from,
        text: m.text,
        ts_unix_ms: m.ts_unix_ms,
        images: m.images.len(),
        pinned,
    }
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

    #[test]
    fn tokens_sum_legacy_hub_counts_and_member_files() {
        let home = tempfile::tempdir().unwrap();
        let hub = store::Hub::create(&hubs_root(home.path()), "api").unwrap();
        hub.update_meta(|meta| {
            meta.tokens.insert("#general".to_string(), 100);
        })
        .unwrap();
        let costs = std::collections::BTreeMap::from([("dm".to_string(), 20)]);
        hub.add_member_tokens("backend", &costs).unwrap();
        hub.add_member_tokens("front", &costs).unwrap();
        assert_eq!(list(home.path()).unwrap()[0].tokens, 140);
        assert_eq!(page(home.path(), "api", None).unwrap().tokens, 140);
    }

    #[test]
    fn page_marks_and_lists_pins_read_only() {
        let home = tempfile::tempdir().unwrap();
        let hub = store::Hub::create(&hubs_root(home.path()), "api").unwrap();
        let id = post(home.path(), "api", "#general", "decided: REST").unwrap();
        post(home.path(), "api", "#general", "chatter").unwrap();
        agent_hub::pin_as_person(&hub, "me", "#general", &id).unwrap();
        let meta_before = hub.meta().unwrap();

        let all = page(home.path(), "api", None).unwrap();
        assert_eq!(all.pinned.len(), 1);
        assert_eq!(all.pinned[0].message.id, id);
        assert_eq!(all.pinned[0].pinned_by, "me");
        let flags: Vec<bool> = all.messages.iter().map(|m| m.pinned).collect();
        assert_eq!(flags, vec![true, false]);
        let general = page(home.path(), "api", Some("general")).unwrap();
        assert_eq!(general.pinned.len(), 1);
        assert_eq!(
            hub.meta().unwrap(),
            meta_before,
            "a page read writes nothing"
        );
    }
}
