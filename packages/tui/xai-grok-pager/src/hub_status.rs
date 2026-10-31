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
        let label = xai_grok_shell::session::hub::status_label(&session_id);
        let Ok(mut cache) = HUB_LABEL_CACHE.lock() else {
            return;
        };
        let is_changed = cache.get(&session_id).is_none_or(|(old, _)| *old != label);
        cache.insert(session_id, (label, Instant::now()));
        drop(cache);
        if is_changed {
            HUB_LABEL_CHANGED.notify_one();
        }
    });
}
