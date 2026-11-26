//! Status-bar label for the agent hub a session is in.
//!
//! The label reads hub files on disk, so it is refreshed off the render
//! thread and cached per session, like [`crate::git_info`]. A refresh that
//! changes a label signals [`label_changed`], so an idle TUI redraws.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

type HubCacheEntry = (Option<String>, Instant);

/// Cached labels, keyed by session id.
static HUB_LABEL_CACHE: LazyLock<Mutex<HashMap<String, HubCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Minimum interval between refreshes of one session's label.
const HUB_LABEL_REFRESH_TTL: Duration = Duration::from_secs(3);

/// Upper bound on cache entries; the pager shows a handful of sessions.
const HUB_LABEL_CACHE_MAX: usize = 64;

/// Signalled when a refresh changes a cached label.
static HUB_LABEL_CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Newest mention time already seen, per hub name. A hub that is seen for the
/// first time records its newest mention, so old mentions do not alert.
static MENTION_HIGH_WATER: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// New mentions that wait for the event loop to show them.
static NEW_MENTIONS: LazyLock<Mutex<Vec<NewMention>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// A mention of the person that arrived after the last refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMention {
    pub hub: String,
    pub mention: xai_grok_shell::session::hub::HubMention,
}

/// Take the mentions that arrived since the last call, oldest first.
pub fn take_new_mentions() -> Vec<NewMention> {
    NEW_MENTIONS
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

/// Purpose: Return the cached hub label of a session, such as
///     `hub api · @backend · 3 unread`.
/// Input: The session id.
/// Output: The label, or `None` when the session is in no named hub.
/// Note: This never blocks. A stale entry starts an off-thread refresh.
pub fn hub_label_lazy(session_id: &str) -> Option<String> {
    let mut cache = HUB_LABEL_CACHE.lock().ok()?;
    let (cached, needs_refresh) = match cache.get(session_id) {
        Some((label, ts)) => (label.clone(), ts.elapsed() >= HUB_LABEL_REFRESH_TTL),
        None => (None, true),
    };
    if needs_refresh {
        if cache.len() >= HUB_LABEL_CACHE_MAX {
            cache.clear();
        }
        cache.insert(session_id.to_string(), (cached.clone(), Instant::now()));
        drop(cache);
        spawn_refresh(session_id.to_string());
    }
    cached
}

/// Signal that a cached label changed and the screen needs a redraw.
pub fn label_changed() -> &'static Notify {
    &HUB_LABEL_CHANGED
}

/// True when some cached session is in a named hub.
pub fn any_session_in_hub() -> bool {
    HUB_LABEL_CACHE
        .lock()
        .is_ok_and(|cache| cache.values().any(|(label, _)| label.is_some()))
}

/// Refresh the label of each session that is in a named hub, so that new
/// unread messages show while the TUI is idle.
pub fn refresh_hub_labels() {
    let session_ids: Vec<String> = match HUB_LABEL_CACHE.lock() {
        Ok(cache) => cache
            .iter()
            .filter(|(_, (label, _))| label.is_some())
            .map(|(id, _)| id.clone())
            .collect(),
        Err(_) => return,
    };
    for session_id in session_ids {
        spawn_refresh(session_id);
    }
}

fn spawn_refresh(session_id: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn_blocking(move || {
        let status = xai_grok_shell::session::hub::hub_status(&session_id);
        let has_new_mentions = status.as_ref().is_some_and(queue_new_mentions);
        let label = status.map(|status| status.label);
        let Ok(mut cache) = HUB_LABEL_CACHE.lock() else {
            return;
        };
        let is_changed = cache.get(&session_id).is_none_or(|(old, _)| *old != label);
        cache.insert(session_id, (label, Instant::now()));
        drop(cache);
        if is_changed || has_new_mentions {
            HUB_LABEL_CHANGED.notify_one();
        }
    });
}

/// Purpose: Queue the mentions of a hub that are newer than the last refresh.
/// Input: The hub status of one session.
/// Output: True when the queue got a mention.
/// Note: Two sessions in one hub share its high-water mark, so a mention
///     alerts once.
fn queue_new_mentions(status: &xai_grok_shell::session::hub::HubStatus) -> bool {
    let newest = status.mentions.iter().map(|m| m.ts_unix_ms).max();
    let Ok(mut high_water) = MENTION_HIGH_WATER.lock() else {
        return false;
    };
    let Some(seen) = high_water.get(&status.hub).copied() else {
        high_water.insert(status.hub.clone(), newest.unwrap_or(0));
        return false;
    };
    let fresh: Vec<NewMention> = status
        .mentions
        .iter()
        .filter(|mention| mention.ts_unix_ms > seen)
        .map(|mention| NewMention {
            hub: status.hub.clone(),
            mention: mention.clone(),
        })
        .collect();
    if fresh.is_empty() {
        return false;
    }
    high_water.insert(status.hub.clone(), newest.unwrap_or(seen));
    drop(high_water);
    if let Ok(mut queue) = NEW_MENTIONS.lock() {
        queue.extend(fresh);
    }
    true
}
