//! Purpose: Hold `SubagentFinished` updates that arrive before their spawn.
//!
//! The shell sends a child's spawn and finish on different paths, so the
//! finish can reach the pager first. Without a matching `SubagentInfo` the
//! finish did nothing, and the spawn that came later left the row "running"
//! forever. This module keeps such a finish until its spawn arrives, and the
//! notification handler then applies it.
//!
//! The buffer is bounded in size and in age, so a finish for a spawn that
//! never comes cannot grow it without limit.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use agent_client_protocol as acp;

/// Most finishes held at the same time, over all children.
const MAX_DEFERRED_FINISHES: usize = 256;

/// Age after which a held finish is dropped.
const DEFERRED_FINISH_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct DeferredFinish {
    notification: acp::ExtNotification,
    deferred_at: Instant,
}

/// Purpose: The finishes held for children whose spawn has not arrived.
/// Note: The key is the child session id. A later finish for the same
/// child replaces the earlier one, because only the last state matters.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeferredSubagentFinishes {
    by_child: HashMap<String, DeferredFinish>,
}

impl DeferredSubagentFinishes {
    pub(crate) fn clear(&mut self) {
        self.by_child.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_child.len()
    }

    /// Purpose: Hold the raw finish notification for `child_session_id`.
    /// Method:
    ///     1. Drop the finishes that are older than the TTL.
    ///     2. If the buffer is full, drop the oldest finish.
    ///     3. Store the finish.
    pub(crate) fn defer(
        &mut self,
        child_session_id: &str,
        notification: acp::ExtNotification,
        now: Instant,
    ) {
        self.prune_expired(now);
        let is_replacement = self.by_child.contains_key(child_session_id);
        if !is_replacement && self.by_child.len() >= MAX_DEFERRED_FINISHES {
            self.evict_oldest();
        }
        self.by_child.insert(
            child_session_id.to_owned(),
            DeferredFinish {
                notification,
                deferred_at: now,
            },
        );
    }

    /// Remove and return the finish held for `child_session_id`, if it has
    /// not expired.
    pub(crate) fn take(
        &mut self,
        child_session_id: &str,
        now: Instant,
    ) -> Option<acp::ExtNotification> {
        self.prune_expired(now);
        self.by_child
            .remove(child_session_id)
            .map(|deferred| deferred.notification)
    }

    fn prune_expired(&mut self, now: Instant) {
        self.by_child.retain(|child_session_id, deferred| {
            let is_live = now.saturating_duration_since(deferred.deferred_at) < DEFERRED_FINISH_TTL;
            if !is_live {
                tracing::debug!(child_session_id, "deferred subagent finish expired");
            }
            is_live
        });
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .by_child
            .iter()
            .min_by_key(|(_, deferred)| deferred.deferred_at)
            .map(|(child_session_id, _)| child_session_id.clone());
        let Some(child_session_id) = oldest else {
            return;
        };
        self.by_child.remove(&child_session_id);
        tracing::debug!(
            child_session_id,
            "deferred subagent finish evicted at capacity"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(tag: &str) -> acp::ExtNotification {
        let params = serde_json::value::to_raw_value(&serde_json::json!({ "tag": tag })).unwrap();
        acp::ExtNotification::new("x.ai/session_notification", params.into())
    }

    fn tag_of(notification: &acp::ExtNotification) -> String {
        let value: serde_json::Value = serde_json::from_str(notification.params.get()).unwrap();
        value["tag"].as_str().unwrap().to_owned()
    }

    #[test]
    fn take_returns_the_held_finish_once() {
        let mut store = DeferredSubagentFinishes::default();
        let now = Instant::now();
        store.defer("child", notification("finish"), now);
        assert_eq!(
            store.take("child", now).as_ref().map(tag_of),
            Some("finish".into())
        );
        assert!(store.take("child", now).is_none());
    }

    #[test]
    fn a_later_finish_replaces_the_earlier_one() {
        let mut store = DeferredSubagentFinishes::default();
        let now = Instant::now();
        store.defer("child", notification("first"), now);
        store.defer("child", notification("second"), now);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.take("child", now).as_ref().map(tag_of),
            Some("second".into())
        );
    }

    #[test]
    fn an_expired_finish_is_not_applied() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        store.defer("child", notification("finish"), start);
        assert!(store.take("child", start + DEFERRED_FINISH_TTL).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn capacity_evicts_the_oldest_finish() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        for index in 0..MAX_DEFERRED_FINISHES {
            store.defer(
                &format!("child-{index}"),
                notification("old"),
                start + Duration::from_millis(index as u64),
            );
        }
        let later = start + Duration::from_secs(1);
        store.defer("child-new", notification("new"), later);
        assert_eq!(store.len(), MAX_DEFERRED_FINISHES);
        assert!(store.take("child-0", later).is_none());
        assert!(store.take("child-new", later).is_some());
    }
}
