//! Status-bar label for the agent hub a session is in.
//!
//! The label reads hub files on disk, so it is refreshed off the render
//! thread and cached per session, like [`crate::git_info`].

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

type HubCacheEntry = (Option<String>, Instant);

/// Cached labels, keyed by session id.
static HUB_LABEL_CACHE: LazyLock<Mutex<HashMap<String, HubCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Minimum interval between refreshes of one session's label.
const HUB_LABEL_REFRESH_TTL: Duration = Duration::from_secs(3);

/// Upper bound on cache entries; the pager shows a handful of sessions.
const HUB_LABEL_CACHE_MAX: usize = 64;

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

fn spawn_refresh(session_id: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn_blocking(move || {
        let label = xai_grok_shell::session::hub::status_label(&session_id);
        if let Ok(mut cache) = HUB_LABEL_CACHE.lock() {
            cache.insert(session_id, (label, Instant::now()));
        }
    });
}
