//! Remove server-rejected images from the stored conversation.
//!
//! The sampler strips a rejected image only from the request that failed.
//! Without this step the image stays in history, so every later turn sends it
//! again, fails again, and pays one more strip-and-retry cycle.

use std::sync::Arc;

use super::ChatStateActor;

impl ChatStateActor {
    /// Purpose: Remove the images with the given URLs from the conversation.
    /// Input: The image URLs that the server rejected.
    /// Output: The number of image parts that the function removed.
    /// Method:
    ///     1. Replace the matched images in place, with the same rules as the
    ///        request-side strip (`strip_images_by_url`).
    ///     2. If an image was removed, rewrite the stored history.
    /// Note: The function does not change the token totals. The provider
    /// total from the last response stays the best estimate, and the next
    /// response corrects it.
    pub(super) fn strip_conversation_images(&mut self, urls: &[Arc<str>]) -> usize {
        let stripped =
            xai_grok_sampling_types::strip_images_by_url(&mut self.state.conversation, urls);
        if stripped == 0 {
            return 0;
        }
        tracing::warn!(
            stripped,
            "stripped server-rejected image(s) from the stored conversation"
        );
        self.persistence.replace_history(&self.state.conversation);
        stripped
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use xai_grok_sampling_types::{
        ContentPart, ConversationItem, IMAGE_STRIP_PLACEHOLDER, SamplingConfig, collect_image_urls,
    };

    use crate::actor::ChatStateActor;
    use crate::handle::ChatStateHandle;
    use crate::persistence::{MockChatPersistence, MockPersistenceReceiver, PersistenceRecord};

    const BAD: &str = "data:image/png;base64,BAD";
    const GOOD: &str = "data:image/png;base64,GOOD";

    fn test_config() -> SamplingConfig {
        SamplingConfig {
            base_url: "https://api.example.com".to_string(),
            model: "test-model".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            adapter_kind: Default::default(),
            request_compat: None,
            endpoint_path: None,
            extra_headers: Default::default(),
            query_params: Default::default(),
            env_http_headers: Default::default(),
            context_window: NonZeroU64::new(128_000).unwrap(),
            reasoning_effort: None,
            stream_tool_calls: None,
        }
    }

    fn spawn_with(
        items: Vec<ConversationItem>,
    ) -> (
        ChatStateHandle,
        MockPersistenceReceiver,
        tokio_util::sync::CancellationToken,
    ) {
        let (mock, persistence_rx) = MockChatPersistence::new();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let token = tokio_util::sync::CancellationToken::new();
        let handle = ChatStateActor::spawn(
            items,
            test_config(),
            Box::new(mock),
            event_tx,
            token.clone(),
        );
        (handle, persistence_rx, token)
    }

    fn user_with_images(urls: &[&str]) -> ConversationItem {
        let mut user = ConversationItem::user("what is in these images?");
        for url in urls {
            user.add_image(url.to_string());
        }
        user
    }

    #[tokio::test]
    async fn stripped_image_is_gone_from_history_and_the_next_request() {
        let items = vec![
            ConversationItem::system("sys"),
            user_with_images(&[BAD, GOOD]),
            ConversationItem::assistant("A test pattern."),
        ];
        let (handle, mut persistence_rx, _token) = spawn_with(items);

        let stripped = handle
            .strip_conversation_images(vec![Arc::<str>::from(BAD)])
            .await;

        assert_eq!(stripped, Some(1));
        let conversation = handle.get_conversation().await;
        assert_eq!(conversation.len(), 3, "the strip keeps every item");
        let urls = collect_image_urls(&conversation);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].as_ref(), GOOD);
        let ConversationItem::User(user) = &conversation[1] else {
            panic!("expected the user item");
        };
        assert!(
            user.content.iter().any(|part| matches!(part,
                ContentPart::Text { text } if text.as_ref() == IMAGE_STRIP_PLACEHOLDER)),
            "the model must see that an image was removed"
        );
        let records = persistence_rx.drain();
        let Some(PersistenceRecord::ReplaceHistory(stored)) = records.last() else {
            panic!("the strip must rewrite the stored history, got {records:?}");
        };
        assert_eq!(collect_image_urls(stored).len(), 1);

        handle.push_user_message(ConversationItem::user("and now?"));
        let next = handle.get_conversation().await;
        assert!(
            collect_image_urls(&next)
                .iter()
                .all(|url| url.as_ref() != BAD),
            "the next turn must not carry the rejected image again"
        );
    }

    #[tokio::test]
    async fn strip_without_a_match_does_not_rewrite_history() {
        let (handle, mut persistence_rx, _token) = spawn_with(vec![user_with_images(&[GOOD])]);

        let stripped = handle
            .strip_conversation_images(vec![Arc::<str>::from(BAD)])
            .await;

        assert_eq!(stripped, Some(0));
        assert!(
            !persistence_rx
                .drain()
                .iter()
                .any(|r| matches!(r, PersistenceRecord::ReplaceHistory(_))),
            "a no-op strip must not rewrite the stored history"
        );
    }
}
