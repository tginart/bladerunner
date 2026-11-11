//! Heal a session whose stored history holds an image that the server rejects.
//!
//! The sampler strips the images from the failed request and retries. It then
//! sends `SamplingEvent::ImagesStripped` for a coded `invalid_image` error.
//! This module decides whether the stored history may also lose the image:
//! - The server's verdict names the request, not one image. Only a request
//!   with exactly one unique image URL gives unambiguous blame, so only then
//!   is the image removed from history. Otherwise the strip stays local to the
//!   request.
//! - The event handler awaits the history rewrite. The turn cannot finish,
//!   and the next prompt cannot read the image again, before the rewrite.

use std::sync::Arc;

use super::*;

impl SessionActor {
    /// Purpose: Remove a server-rejected image from the stored conversation.
    /// Input: The image URLs that the sampler stripped from the request.
    /// Output: None. The function tells the user what happened.
    /// Method:
    ///     1. Find the one image to blame. If there is none, keep the strip
    ///        local to the request.
    ///     2. Remove that image from the chat state. The chat state also queues a
    ///        rewrite of the stored history.
    ///     3. Send a notice that matches the result.
    pub(crate) async fn handle_images_stripped(&self, stripped_urls: Vec<Arc<str>>) {
        let stripped = stripped_urls.len();
        let Some(blamed) = single_blamed_image(stripped_urls) else {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                stripped,
                "server rejected a request with several distinct images; strip stays request-local"
            );
            self.send_image_strip_notice(format!(
                "The server could not process an image in this request; {stripped} image(s) \
                 were left out of the retry."
            ))
            .await;
            return;
        };
        let removed = self
            .chat_state_handle
            .strip_conversation_images(vec![blamed])
            .await
            .unwrap_or(0);
        tracing::warn!(
            session_id = %self.session_info.id.0,
            stripped,
            removed,
            "server-rejected image strip"
        );
        // The history rewrite is queued, not acknowledged, so the notice
        // claims only what is sure: later requests leave the image out.
        let notice = if removed > 0 {
            "The server could not process an image, so later requests in this session \
             leave it out. Re-attach it if it is still needed."
        } else {
            "The server could not process an image, so it was left out of this request."
        };
        self.send_image_strip_notice(notice.to_string()).await;
    }

    async fn send_image_strip_notice(&self, note: String) {
        self.send_xai_notification(XaiSessionUpdate::ImageDropped { notes: vec![note] })
            .await;
    }
}

/// The only unique URL in `urls`, or `None` when there are zero or several.
/// The same image attached twice is still one suspect.
fn single_blamed_image(urls: Vec<Arc<str>>) -> Option<Arc<str>> {
    let mut urls = urls.into_iter();
    let first = urls.next()?;
    if urls.any(|url| url != first) {
        return None;
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(values: &[&str]) -> Vec<Arc<str>> {
        values.iter().map(|v| Arc::<str>::from(*v)).collect()
    }

    #[test]
    fn single_blamed_image_needs_exactly_one_unique_url() {
        assert_eq!(single_blamed_image(urls(&[])), None);
        assert_eq!(
            single_blamed_image(urls(&["a"])).as_deref(),
            Some("a"),
            "one image is unambiguous"
        );
        assert_eq!(
            single_blamed_image(urls(&["a", "a"])).as_deref(),
            Some("a"),
            "the same image twice is one suspect"
        );
        assert_eq!(
            single_blamed_image(urls(&["a", "b"])),
            None,
            "distinct images make the blame ambiguous"
        );
    }
}
