//! Production `ChatPersistence` implementation backed by the existing persistence channel.
//!
//! Wraps an `mpsc::UnboundedSender<PersistenceMsg>` and translates
//! `ChatPersistence` trait calls into the appropriate `PersistenceMsg` variants.

use std::{io, sync::Arc};

use tokio::sync::{mpsc, oneshot};
use xai_chat_state::{ChatPersistence, StrictAppendAck, StrictAppendError};
use xai_grok_sampling_types::ConversationItem;

use super::persistence::PersistenceMsg;

/// Production `ChatPersistence` that sends to the existing session persistence channel.
///
/// Translates:
/// - `persist_message` → `PersistenceMsg::Chat`
/// - `persist_working_directory_switch_and_ack` → `PersistenceMsg::AppendCwdSwitchAndAck`
/// - `replace_history` → `PersistenceMsg::ReplaceChatHistory`
/// - `flush` → `PersistenceMsg::Flush`
pub(crate) struct ChannelChatPersistence {
    tx: mpsc::UnboundedSender<PersistenceMsg>,
}

#[derive(Debug)]
struct ChannelEvidenceSink {
    tx: mpsc::UnboundedSender<PersistenceMsg>,
}

impl xai_grok_sampling_types::RequestEvidenceSink for ChannelEvidenceSink {
    fn record(&self, evidence: &xai_grok_sampling_types::RequestEvidence) {
        let (respond_to, _written) = oneshot::channel();
        let _ = self.tx.send(PersistenceMsg::RequestEvidence {
            tools: Some(evidence.tools.clone()),
            system: evidence.system.clone(),
            respond_to,
        });
    }
}

pub(crate) fn request_evidence_sink(
    tx: mpsc::UnboundedSender<PersistenceMsg>,
) -> Arc<dyn xai_grok_sampling_types::RequestEvidenceSink> {
    Arc::new(ChannelEvidenceSink { tx })
}

impl ChannelChatPersistence {
    /// Create a new `ChannelChatPersistence` wrapping the given persistence channel.
    pub(crate) fn new(tx: mpsc::UnboundedSender<PersistenceMsg>) -> Self {
        Self { tx }
    }
}

impl ChatPersistence for ChannelChatPersistence {
    fn persist_message(&mut self, item: &ConversationItem) {
        let _ = self.tx.send(PersistenceMsg::Chat(item.clone()));
    }

    fn persist_working_directory_switch_and_ack(
        &mut self,
        item: &ConversationItem,
    ) -> oneshot::Receiver<Result<StrictAppendAck, StrictAppendError>> {
        let (reply, receiver) = oneshot::channel();
        if self
            .tx
            .send(PersistenceMsg::AppendCwdSwitchAndAck {
                item: item.clone(),
                respond_to: reply,
            })
            .is_err()
        {
            let (reply, receiver) = oneshot::channel();
            let _ = reply.send(Err(StrictAppendError::Indeterminate(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor unavailable; retry by generation",
            ))));
            return receiver;
        }
        receiver
    }

    fn replace_history(&mut self, items: &[ConversationItem]) {
        let _ = self
            .tx
            .send(PersistenceMsg::ReplaceChatHistory(items.to_vec()));
    }

    fn flush(&mut self) {
        let _ = self.tx.send(PersistenceMsg::Flush);
    }

    fn persist_request_evidence(
        &mut self,
        request: &mut xai_grok_sampling_types::ConversationRequest,
    ) {
        request.request_evidence_sink = Some(request_evidence_sink(self.tx.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn evidence_is_recorded_after_final_request_preparation_and_survives_retries() {
        use xai_grok_sampling_types::{RequestEvidence, ToolSpec};
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        let mut request = xai_grok_sampling_types::ConversationRequest::from_items(vec![
            ConversationItem::system("original"),
        ]);
        persistence.persist_request_evidence(&mut request);
        assert!(
            rx.try_recv().is_err(),
            "building a request cannot persist preliminary evidence"
        );
        request.items[0] = ConversationItem::system("effective instructions");
        request.tools.push(ToolSpec {
            name: "effective_tool".into(),
            description: None,
            parameters: serde_json::json!({}),
        });
        let evidence = RequestEvidence::new(&request.tools, Some("effective instructions".into()));
        let retry = request.clone();
        for attempt in [&request, &retry] {
            attempt
                .request_evidence_sink
                .as_ref()
                .unwrap()
                .record(&evidence);
            let PersistenceMsg::RequestEvidence {
                tools,
                system,
                respond_to,
            } = rx.try_recv().unwrap()
            else {
                panic!("expected evidence");
            };
            assert_eq!(tools, Some(evidence.tools.clone()));
            assert_eq!(system, evidence.system);
            drop(respond_to); // Missing acknowledgement cannot suppress the retry.
        }
    }

    #[tokio::test]
    async fn channel_persistence_sends_chat_messages() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        let item = ConversationItem::user("test");
        persistence.persist_message(&item);
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, PersistenceMsg::Chat(_)));
    }

    #[tokio::test]
    async fn channel_persistence_sends_acknowledged_chat_append() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        let item = ConversationItem::working_directory_switch("moved", 1);
        let ack = persistence.persist_working_directory_switch_and_ack(&item);
        let msg = rx.recv().await.unwrap();
        let PersistenceMsg::AppendCwdSwitchAndAck {
            item: persisted,
            respond_to,
        } = msg
        else {
            panic!("expected acknowledged chat append");
        };
        assert_eq!(
            serde_json::to_vec(&persisted).unwrap(),
            serde_json::to_vec(&item).unwrap()
        );
        respond_to.send(Ok(StrictAppendAck::Appended)).unwrap();
        assert!(matches!(
            ack.await.unwrap().unwrap(),
            StrictAppendAck::Appended
        ));
    }

    #[tokio::test]
    async fn channel_persistence_sends_replace_history() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        persistence.replace_history(&[ConversationItem::system("compacted")]);
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, PersistenceMsg::ReplaceChatHistory(_)));
    }

    #[tokio::test]
    async fn channel_persistence_sends_flush() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut persistence = ChannelChatPersistence::new(tx);
        persistence.flush();
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, PersistenceMsg::Flush));
    }
}
