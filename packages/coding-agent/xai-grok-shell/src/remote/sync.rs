//! Writeback push: async queue that flushes session updates to the backend.
//!
//! `RemoteSync` runs a background tokio task that buffers ACP notifications
//! and flushes them to the backend via [`BackendClient::save_session_data()`].
//!
//! ## Backpressure
//!
//! When the buffer exceeds [`MAX_PENDING`], the task attempts an emergency
//! flush. If that also fails (network down), the oldest messages are dropped
//! to prevent unbounded memory growth.
//!
//! ## Drop behavior
//!
//! When `RemoteSync` is dropped, the sender half of the channel closes and
//! the background task exits. **Pending buffered messages are lost.** This
//! is acceptable because the local JSONL files are the source of truth —
//! writeback is best-effort.

use crate::remote::BackendClient;
use crate::session::export::{ExportedMessage, ExportedMetadata};
use agent_client_protocol as acp;
use tokio::sync::mpsc;
use xai_grok_telemetry::id::agent_id;

/// Max buffered notifications before triggering an emergency flush.
/// Sized to keep memory under ~50MB even with large notifications.
const MAX_PENDING: usize = 512;

/// How many oldest messages to drop when an emergency flush fails.
/// Dropping a batch (not one-by-one) avoids repeated failed flushes.
const DROP_BATCH_SIZE: usize = 64;

enum SyncMsg {
    Queue(Box<acp::SessionNotification>),
    Provenance(
        acp::SessionId,
        Box<crate::remote::provenance::RequestProvenance>,
    ),
    Flush,
    SetTitle {
        title: String,
        is_manual: bool,
    },
    /// Drop cached title + manual flag so later flushes cannot re-advertise
    /// a pin the local summary no longer has.
    ClearTitle,
    SetModelId(String),
}

#[derive(Clone)]
pub struct RemoteSync {
    tx: mpsc::UnboundedSender<SyncMsg>,
}

impl RemoteSync {
    #[cfg(test)]
    pub(crate) fn test_observer() -> (Self, mpsc::UnboundedReceiver<acp::SessionNotification>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (observed_tx, observed_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let SyncMsg::Queue(notification) = message {
                    let _ = observed_tx.send(*notification);
                }
            }
        });
        (Self { tx }, observed_rx)
    }

    /// Metadata is included on every flush to keep the backend session row current.
    pub(crate) fn new(
        session_id: String,
        metadata: ExportedMetadata,
        client: BackendClient,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(sync_task(session_id, metadata, client, rx));
        Self { tx }
    }

    pub fn queue(&self, notification: acp::SessionNotification) {
        let _ = self.tx.send(SyncMsg::Queue(Box::new(notification)));
    }

    pub(crate) fn queue_provenance(
        &self,
        session_id: acp::SessionId,
        record: crate::remote::provenance::RequestProvenance,
    ) {
        let _ = self
            .tx
            .send(SyncMsg::Provenance(session_id, Box::new(record)));
    }

    pub fn flush(&self) {
        let _ = self.tx.send(SyncMsg::Flush);
    }

    pub fn set_title(&self, title: String) {
        let _ = self.tx.send(SyncMsg::SetTitle {
            title,
            is_manual: false,
        });
    }

    pub fn set_manual_title(&self, title: String) {
        let _ = self.tx.send(SyncMsg::SetTitle {
            title,
            is_manual: true,
        });
    }

    pub fn clear_title(&self) {
        let _ = self.tx.send(SyncMsg::ClearTitle);
    }

    pub(crate) fn set_model_id(&self, model_id: String) {
        let _ = self.tx.send(SyncMsg::SetModelId(model_id));
    }
}

async fn do_flush(
    client: &BackendClient,
    session_id: &str,
    metadata: &ExportedMetadata,
    pending: &mut Vec<ExportedMessage>,
) -> bool {
    if pending.is_empty() {
        return true;
    }

    // Bound each POST, including JSON escaping of the exported JSON-RPC strings.
    // Drain successful prefixes immediately so a later failure never duplicates them.
    let result = flush_batches(pending, |batch| async move {
        client
            .save_session_data(session_id, &batch, Some(metadata))
            .await
    })
    .await;
    if result {
        if let Err(e) = client
            .upsert_session(session_id, metadata, &agent_id())
            .await
        {
            tracing::warn!(error = %e, "Writeback: failed to upsert session");
        }
    }
    result
}

async fn flush_batches<F, Fut>(pending: &mut Vec<ExportedMessage>, mut send: F) -> bool
where
    F: FnMut(Vec<ExportedMessage>) -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::remote::BackendError>>,
{
    const BATCH_BYTES: usize = 64 * 1024;
    while !pending.is_empty() {
        let mut count = 0;
        let mut bytes = 0;
        for message in pending.iter().take(64) {
            let size = serde_json::to_vec(message).map_or(BATCH_BYTES, |v| v.len());
            if count > 0 && bytes + size > BATCH_BYTES {
                break;
            }
            bytes += size;
            count += 1;
        }
        loop {
            match send(pending[..count].to_vec()).await {
                Ok(()) => {
                    pending.drain(..count);
                    break;
                }
                Err(crate::remote::BackendError::RequestFailed { status: 413, .. })
                    if count > 1 =>
                {
                    count = count.div_ceil(2);
                }
                Err(error) => {
                    tracing::warn!(%error, pending = pending.len(), "Writeback: flush failed");
                    return false;
                }
            }
        }
    }
    true
}

async fn sync_task(
    session_id: String,
    mut metadata: ExportedMetadata,
    client: BackendClient,
    mut rx: mpsc::UnboundedReceiver<SyncMsg>,
) {
    let mut pending: Vec<ExportedMessage> = Vec::new();
    let mut provenance = crate::remote::provenance::ProvenanceEncoder::default();

    while let Some(msg) = rx.recv().await {
        if pending.len() >= MAX_PENDING {
            metadata.updated_at = Some(chrono::Utc::now().to_rfc3339());
            if !do_flush(&client, &session_id, &metadata, &mut pending).await {
                let dropped = pending.drain(0..DROP_BATCH_SIZE.min(pending.len())).count();
                provenance.reset();
                tracing::error!(
                    dropped,
                    "Writeback: emergency flush failed, dropping oldest messages"
                );
            }
        }
        match msg {
            SyncMsg::Queue(n) => {
                pending.push(ExportedMessage::from_notification(&n));
            }
            SyncMsg::Provenance(session_id, record) => match provenance.encode(*record) {
                Ok(chunks) => {
                    for chunk in chunks {
                        let notification = crate::extensions::notification::SessionNotification {
                                session_id: session_id.clone(),
                                update: crate::extensions::notification::SessionUpdate::RequestProvenanceChunk(Box::new(chunk)),
                                meta: None,
                            };
                        pending.push(ExportedMessage::from_xai_notification(&notification));
                    }
                }
                Err(error) => tracing::warn!(%error, "Writeback: cannot encode provenance"),
            },
            SyncMsg::Flush => {
                metadata.updated_at = Some(chrono::Utc::now().to_rfc3339());
                do_flush(&client, &session_id, &metadata, &mut pending).await;
            }
            SyncMsg::SetTitle { title, is_manual } => {
                metadata.title = Some(title);
                metadata.title_is_manual = is_manual.then_some(true);
                metadata.updated_at = Some(chrono::Utc::now().to_rfc3339());
                if let Err(e) = client
                    .save_session_data(&session_id, &[], Some(&metadata))
                    .await
                {
                    tracing::warn!(?e, "Writeback: failed to sync title to backend");
                } else if let Err(e) = client
                    .upsert_session(&session_id, &metadata, &agent_id())
                    .await
                {
                    // save_session_data does not write the session-row title
                    // (backend upsert title=None). Without this, list/--resume
                    // keep the pre-rename row until the next message flush.
                    tracing::warn!(error = %e, "Writeback: failed to upsert session title");
                }
            }
            SyncMsg::ClearTitle => {
                // Empty string, not `None`: an omitted field leaves the
                // backend's prior pin in place.
                metadata.title = Some(String::new());
                metadata.title_is_manual = Some(false);
                metadata.updated_at = Some(chrono::Utc::now().to_rfc3339());
                if let Err(e) = client
                    .save_session_data(&session_id, &[], Some(&metadata))
                    .await
                {
                    tracing::warn!(?e, "Writeback: failed to clear title on backend");
                } else if let Err(e) = client
                    .upsert_session(&session_id, &metadata, &agent_id())
                    .await
                {
                    // Same row-title gap as SetTitle: save_session_data does
                    // not clear the session-row pin (backend title=None).
                    tracing::warn!(error = %e, "Writeback: failed to upsert cleared session title");
                }
            }
            SyncMsg::SetModelId(id) => {
                metadata.model_id = Some(id);
                metadata.updated_at = Some(chrono::Utc::now().to_rfc3339());
                if let Err(e) = client
                    .save_session_data(&session_id, &[], Some(&metadata))
                    .await
                {
                    tracing::warn!(?e, "Writeback: failed to sync model_id to backend");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::BackendError;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn oversized_batches_back_off_and_successful_prefixes_are_not_retried() {
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let mut pending: Vec<_> = (0..30)
            .map(|n| ExportedMessage {
                content: format!("{n}:{}", "x".repeat(8000)),
                timestamp: None,
            })
            .collect();
        let expected: Vec<_> = pending.iter().map(|m| m.content.clone()).collect();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(
            !flush_batches(&mut pending, {
                let accepted = accepted.clone();
                let calls = calls.clone();
                move |batch| {
                    let accepted = accepted.clone();
                    let calls = calls.clone();
                    async move {
                        if serde_json::to_vec(&batch).unwrap().len() > 20_000 {
                            return Err(BackendError::RequestFailed {
                                status: 413,
                                body: String::new(),
                            });
                        }
                        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n == 3 {
                            return Err(BackendError::RequestFailed {
                                status: 503,
                                body: String::new(),
                            });
                        }
                        accepted
                            .lock()
                            .unwrap()
                            .extend(batch.into_iter().map(|m| m.content));
                        Ok(())
                    }
                }
            })
            .await
        );
        assert_eq!(pending.len(), 24);
        assert!(
            flush_batches(&mut pending, {
                let accepted = accepted.clone();
                move |batch| {
                    let accepted = accepted.clone();
                    async move {
                        if serde_json::to_vec(&batch).unwrap().len() > 20_000 {
                            return Err(BackendError::RequestFailed {
                                status: 413,
                                body: String::new(),
                            });
                        }
                        accepted
                            .lock()
                            .unwrap()
                            .extend(batch.into_iter().map(|m| m.content));
                        Ok(())
                    }
                }
            })
            .await
        );
        assert!(pending.is_empty());
        assert_eq!(*accepted.lock().unwrap(), expected);
    }
}
