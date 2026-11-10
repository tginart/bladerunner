//! Replay pipeline for cross-compaction rewind.
//!
//! When rewinding to a prompt that precedes a compaction boundary, the in-memory
//! conversation (and `chat_history.jsonl`) no longer contains the original
//! messages. This module reconstructs the conversation by streaming
//! `updates.jsonl` and handling `CompactionCheckpoint` / `RewindMarker` entries.

use std::io;
use std::path::Path;

use crate::extensions::notification::{
    CompactionCheckpointFile, CompactionCheckpointInfo, SessionUpdate as XaiSessionUpdate,
};
use crate::sampling::ConversationItem;
use crate::session::storage::{SessionUpdate, UpdatesIterator};

/// Result of replaying updates.jsonl up to a target prompt index.
#[derive(Debug)]
pub struct ReplayResult {
    /// The reconstructed conversation, suitable for replacing in-memory state.
    pub conversation: Vec<ConversationItem>,
    /// The prompt index that was reached (should equal the target).
    pub prompt_index_reached: usize,
    /// The original User(user_info) text from before the first compaction.
    /// Extracted from the checkpoint file's `original_user_info` field.
    /// `None` if no checkpoint was encountered or the checkpoint predates
    /// the field (schema_version 1 without it).
    pub original_user_info: Option<String>,
    /// Compaction marker for the rebuilt conversation: `Some(idx)` if a summary survives, else `None`.
    pub last_compaction_prompt_index: Option<usize>,
}

/// Find the most recent `CompactionCheckpoint` in `updates.jsonl`.
///
/// Uses raw-line peeking: only lines containing `"compaction_checkpoint"`
/// are parsed, skipping full typed deserialization.
pub fn find_latest_compaction_checkpoint(
    updates_path: &Path,
) -> io::Result<Option<CompactionCheckpointInfo>> {
    use crate::session::storage::RawLinePeek;

    let raw_contents = match std::fs::read_to_string(updates_path) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    if !raw_contents.contains("compaction_checkpoint") {
        return Ok(None);
    }

    let mut latest: Option<CompactionCheckpointInfo> = None;

    for line in raw_contents.lines() {
        if line.trim().is_empty() || !line.contains("compaction_checkpoint") {
            continue;
        }

        let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line) else {
            continue;
        };

        if env.method != Some("_x.ai/session/update") {
            continue;
        }

        let Some(raw_params) = env.params else {
            continue;
        };

        if let Ok(notification) = serde_json::from_str::<
            crate::extensions::notification::SessionNotification,
        >(raw_params.get())
            && let XaiSessionUpdate::CompactionCheckpoint(info) = notification.update
        {
            latest = Some(*info);
        }
    }

    Ok(latest)
}

/// Replay `updates.jsonl` to reconstruct the conversation at `target_prompt_index`.
///
/// This handles:
/// - `RewindMarker`: discards accumulated state beyond the marker's target.
/// - `CompactionCheckpoint`: loads/ignores checkpoints based on whether the
///   target is before or after the compaction boundary.
///
/// `session_dir` is the path to the session directory (for reading checkpoint files).
///
/// # Errors
/// Fails only when the base checkpoint for the target is unreadable. An older
/// checkpoint that a later one supersedes, or one that a rewind marker
/// discards, may be missing (the 30-day sweep can delete it).
pub fn replay_to_prompt(
    updates_path: &Path,
    session_dir: &Path,
    target_prompt_index: usize,
) -> io::Result<ReplayResult> {
    let Some(iter) = UpdatesIterator::open(updates_path)? else {
        return Ok(ReplayResult {
            conversation: vec![],
            prompt_index_reached: 0,
            original_user_info: None,
            last_compaction_prompt_index: None,
        });
    };

    let mut state = ReplayState::new(target_prompt_index);

    for update_result in iter {
        let update = match update_result {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(?e, "Skipping malformed update during replay");
                continue;
            }
        };

        let action = state.process_update(&update, session_dir)?;
        if action == ReplayAction::Stop {
            break;
        }
    }

    // Flush any trailing partial messages.
    state.flush_pending_user();
    state.flush_pending_agent();

    if let Some(Base { blob: Err(e), .. }) = state.bases.pop_if(|base| base.blob.is_err()) {
        return Err(e);
    }

    // After processing the entire file, the conversation may extend beyond
    // the target. `target_prompt_index` means "rewind to before prompt N",
    // so keep prompts 0..N-1 (N prompts total).
    if state.prompt_counter > target_prompt_index {
        if let Some(top) = state.bases.last()
            && target_prompt_index >= top.prompt_index
        {
            let (base_len, base_index) = (top.base_len, top.prompt_index);
            state.truncate_after_base(base_len, target_prompt_index - base_index);
        } else if target_prompt_index == 0 {
            state.conversation.clear();
        } else {
            let truncate_at = state.truncate_target(target_prompt_index);
            let keep =
                crate::sampling::conversation_truncate_for_prompt(&state.conversation, truncate_at);
            state.conversation.truncate(keep);
        }
        state.prompt_counter = target_prompt_index;
    }

    Ok(ReplayResult {
        conversation: state.conversation,
        prompt_index_reached: state.prompt_counter,
        original_user_info: state.original_user_info,
        last_compaction_prompt_index: state.bases.last().map(|base| base.prompt_index),
    })
}

/// One compaction checkpoint installed as the conversation base.
/// `blob: Err` means that its file was unreadable: counting goes on as if it
/// loaded, and the error surfaces only if the base is still on top at EOF.
struct Base {
    prompt_index: usize,
    /// Conversation length right after the install. Items below it are the
    /// opaque base and are never counted or truncated.
    base_len: usize,
    blob: io::Result<()>,
}

#[derive(Debug, PartialEq)]
enum ReplayAction {
    Continue,
    Stop,
}

/// Internal state machine for the replay pipeline.
struct ReplayState {
    /// The prompt index we're trying to reach.
    target: usize,

    /// Accumulated conversation items.
    conversation: Vec<ConversationItem>,

    /// Current prompt counter (how many user turns we've seen).
    prompt_counter: usize,

    /// Whether we're inside a contiguous sequence of UserMessageChunk updates
    /// (used to count user turns correctly — multiple chunks = one turn).
    in_user_message: bool,

    /// Partial text accumulator for the current user message.
    current_user_text: String,

    current_user_prompt_index: Option<usize>,

    /// True once any user chunk with `_meta.promptIndex` has been seen.
    /// Unnumbered user runs after that are mid-turn phantoms (not turns).
    seen_prompt_index_marker: bool,

    /// Partial text accumulator for the current agent message.
    current_agent_text: String,

    /// Whether there's a pending agent message to flush.
    has_pending_agent: bool,

    /// Installed checkpoint bases, innermost last. A loaded checkpoint clears
    /// the stack, an unreadable one stacks on top, and a rewind marker pops
    /// every base above its target. While the stack is not empty, only real
    /// `UserMessageChunk` turns count; the users inside a blob do not.
    bases: Vec<Base>,

    /// The original User(user_info) text from before the first compaction.
    original_user_info: Option<String>,
}

impl ReplayState {
    fn new(target: usize) -> Self {
        Self {
            target,
            conversation: Vec::new(),
            prompt_counter: 0,
            in_user_message: false,
            current_user_text: String::new(),
            current_user_prompt_index: None,
            seen_prompt_index_marker: false,
            current_agent_text: String::new(),
            has_pending_agent: false,
            bases: Vec::new(),
            original_user_info: None,
        }
    }

    fn process_update(
        &mut self,
        update: &SessionUpdate,
        session_dir: &Path,
    ) -> io::Result<ReplayAction> {
        match update {
            SessionUpdate::Xai(notification) => {
                match &notification.update {
                    XaiSessionUpdate::CompactionCheckpoint(info) => {
                        self.handle_checkpoint(info, session_dir);
                        return Ok(ReplayAction::Continue);
                    }
                    XaiSessionUpdate::RewindMarker {
                        target_prompt_index,
                        ..
                    } => {
                        self.handle_rewind_marker(*target_prompt_index);
                        return Ok(ReplayAction::Continue);
                    }
                    // Other xAI notifications are informational — skip them.
                    _ => {}
                }
            }
            SessionUpdate::Acp(notification) => {
                match &notification.update {
                    agent_client_protocol::SessionUpdate::UserMessageChunk(chunk) => {
                        return Ok(self.handle_user_chunk(chunk));
                    }
                    agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
                        self.handle_agent_chunk(chunk);
                    }
                    _ => {
                        // Other ACP updates (ToolCall, StatusUpdate, etc.)
                        // don't affect prompt counting — skipped in replay.
                    }
                }
            }
        }
        Ok(ReplayAction::Continue)
    }

    /// Install a checkpoint as the conversation base, or note it for its
    /// `original_user_info` when the target is before it. Never fails: an
    /// unreadable checkpoint becomes a `blob: Err` base that a later
    /// checkpoint or rewind marker can still supersede.
    fn handle_checkpoint(&mut self, info: &CompactionCheckpointInfo, session_dir: &Path) {
        let checkpoint_path = session_dir.join(&info.checkpoint_file);
        let compaction_at = info.prompt_index_at_compaction;

        if self.target < compaction_at {
            // The target is before this compaction, so raw updates rebuild the
            // conversation. The checkpoint only supplies the historical
            // User(user_info) that the model saw for those turns.
            match read_checkpoint_file(&checkpoint_path) {
                Ok(file) => {
                    if self.original_user_info.is_none() {
                        self.original_user_info = file.original_user_info;
                    }
                }
                Err(e) => tracing::warn!(
                    ?e,
                    compaction_at,
                    target = self.target,
                    "checkpoint unreadable; pre-compaction rewind keeps the current user_info"
                ),
            }
            return;
        }

        self.in_user_message = false;
        self.current_user_text.clear();
        self.current_user_prompt_index = None;
        self.current_agent_text.clear();
        self.has_pending_agent = false;
        // Count as if the blob loaded, so later markers and checkpoints
        // resolve the same way either way.
        self.prompt_counter = compaction_at;

        match read_checkpoint_file(&checkpoint_path) {
            Ok(file) => self.install_loaded_base(info, file),
            Err(e) => {
                tracing::warn!(
                    ?e,
                    compaction_at,
                    "checkpoint unreadable; rewind fails unless a later checkpoint or rewind \
                     marker supersedes it"
                );
                let user_error = unreadable_checkpoint_error(&e, info);
                // An unreadable checkpoint never replaced the conversation, so
                // the base below it stays.
                self.bases.push(Base {
                    prompt_index: compaction_at,
                    base_len: self.conversation.len(),
                    blob: Err(user_error),
                });
            }
        }
    }

    fn install_loaded_base(
        &mut self,
        info: &CompactionCheckpointInfo,
        file: CompactionCheckpointFile,
    ) {
        // handle_rewind needs original_user_info for the raw-updates prefix
        // case even when the conversation is replaced.
        if self.original_user_info.is_none() {
            self.original_user_info = file.original_user_info;
        }
        self.conversation = file.compacted_history;
        // Checkpoints predate this binary's validation (or the API's
        // current validators), so heal them like the jsonl loader does
        // — otherwise a cross-compaction rewind re-injects a stripped
        // poison image and every turn 400s until the next restart.
        let stripped_images =
            crate::session::storage::jsonl::strip_invalid_images(&mut self.conversation);
        if stripped_images > 0 {
            tracing::warn!(
                count = stripped_images,
                "stripped invalid images from compaction checkpoint history"
            );
        }
        self.bases.clear();
        self.bases.push(Base {
            prompt_index: info.prompt_index_at_compaction,
            base_len: self.conversation.len(),
            blob: Ok(()),
        });
        tracing::debug!(
            prompt_counter = self.prompt_counter,
            "Replay: loaded compaction checkpoint"
        );
        // If the auto-continue prompt was recorded, add it as a user turn.
        // prompt_counter already equals prompt_index_at_compaction.
        if let Some(ref ac) = info.auto_continue {
            self.conversation
                .push(ConversationItem::user(ac.prompt_text.clone()));
        }
    }

    fn handle_rewind_marker(&mut self, marker_target: usize) {
        // Discard any in-progress partial messages — they belong to the
        // timeline being discarded, so we drop them rather than flushing.
        self.current_user_text.clear();
        self.current_user_prompt_index = None;
        self.current_agent_text.clear();
        self.has_pending_agent = false;
        self.in_user_message = false;

        if self.prompt_counter <= marker_target {
            return;
        }

        // `marker_target = N` means "rewind to before prompt N", keeping
        // prompts 0..N-1 (N prompts total). Bases above the target belong to
        // the discarded timeline, readable or not.
        while self
            .bases
            .last()
            .is_some_and(|base| base.prompt_index > marker_target)
        {
            self.bases.pop();
        }
        if let Some(top) = self.bases.last() {
            // Keep the base blob intact and discard only later user turns.
            let (base_len, base_index) = (top.base_len, top.prompt_index);
            self.truncate_after_base(base_len, marker_target - base_index);
        } else if marker_target == 0 {
            // Rewind to the very beginning — discard everything.
            self.conversation.clear();
        } else {
            // No base left. A popped loaded blob still heads the conversation
            // and is truncated like raw items.
            let truncate_at = self.truncate_target(marker_target);
            let keep =
                crate::sampling::conversation_truncate_for_prompt(&self.conversation, truncate_at);
            self.conversation.truncate(keep);
        }

        self.prompt_counter = marker_target;
    }

    /// Keep the base blob and only the first `turns_to_keep` real user turns
    /// after it.
    fn truncate_after_base(&mut self, base_len: usize, turns_to_keep: usize) {
        let mut seen_marker = false;
        let mut user_count = 0;
        let mut cut_pos = self.conversation.len();
        if let Some(tail) = self.conversation.get(base_len..) {
            for (i, item) in tail.iter().enumerate() {
                if counts_as_replay_turn_progressive(item, &mut seen_marker) {
                    user_count += 1;
                    if user_count > turns_to_keep {
                        cut_pos = base_len + i;
                        break;
                    }
                }
            }
        }
        self.conversation.truncate(cut_pos);
    }

    fn handle_user_chunk(&mut self, chunk: &agent_client_protocol::ContentChunk) -> ReplayAction {
        if crate::session::storage::is_host_turn_chunk(chunk) {
            self.flush_host_turn_boundary();
            return ReplayAction::Continue;
        }
        let chunk_prompt_index = chunk
            .meta
            .as_ref()
            .and_then(|m| m.get("promptIndex"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        if chunk_prompt_index.is_some() {
            self.seen_prompt_index_marker = true;
        }

        if !self.in_user_message {
            self.flush_pending_agent();
            self.in_user_message = true;
            self.current_user_text.clear();
            self.current_user_prompt_index = chunk_prompt_index;
        } else if chunk_prompt_index != self.current_user_prompt_index
            && (chunk_prompt_index.is_some() || self.current_user_prompt_index.is_some())
        {
            // New run: promptIndex changed, or transition between marked/unmarked.
            self.flush_pending_user();
            self.in_user_message = true;
            self.current_user_text.clear();
            self.current_user_prompt_index = chunk_prompt_index;
        } else if self.current_user_prompt_index.is_none() {
            self.current_user_prompt_index = chunk_prompt_index;
        }

        if let agent_client_protocol::ContentBlock::Text(t) = &chunk.content {
            self.current_user_text.push_str(&t.text);
        }

        // Note: we do NOT early-stop when prompt_counter > target because a
        // later RewindMarker could reset the counter back below the target.
        // The replay processes the entire file and the final conversation
        // state is correct regardless of timeline branches.
        ReplayAction::Continue
    }

    fn handle_agent_chunk(&mut self, chunk: &agent_client_protocol::ContentChunk) {
        if crate::session::storage::is_host_turn_chunk(chunk) {
            self.flush_host_turn_boundary();
            return;
        }

        // An agent chunk ends any in-progress user message.
        if self.in_user_message {
            self.flush_pending_user();
            self.in_user_message = false;
        }

        if let agent_client_protocol::ContentBlock::Text(t) = &chunk.content {
            self.current_agent_text.push_str(&t.text);
            self.has_pending_agent = true;
        }
    }

    fn flush_host_turn_boundary(&mut self) {
        if self.in_user_message {
            self.flush_pending_user();
            self.in_user_message = false;
        }
        self.flush_pending_agent();
    }

    fn conversation_has_markers(&self) -> bool {
        self.seen_prompt_index_marker
            || self
                .conversation
                .iter()
                .any(|i| matches!(i, ConversationItem::User(u) if u.prompt_index.is_some()))
    }

    /// Absolute `target` when items carry `prompt_index`; else `target - 1`
    /// for the preamble-aware counting fallback.
    fn truncate_target(&self, target: usize) -> usize {
        if self.conversation_has_markers() {
            target
        } else {
            target.saturating_sub(1)
        }
    }

    fn flush_pending_user(&mut self) {
        if self.current_user_text.is_empty() {
            self.current_user_prompt_index = None;
            return;
        }
        let text = std::mem::take(&mut self.current_user_text);
        let pi = self.current_user_prompt_index.take();
        if let Some(pi) = pi {
            let mut item = ConversationItem::user(text);
            item.set_prompt_index(pi);
            self.conversation.push(item);
            self.prompt_counter += 1;
        } else if !self.seen_prompt_index_marker {
            self.conversation.push(ConversationItem::user(text));
            self.prompt_counter += 1;
        } else {
            // Mid-turn phantom after markers: keep text, do not count.
            self.conversation.push(ConversationItem::user(text));
        }
    }

    fn flush_pending_agent(&mut self) {
        if self.has_pending_agent {
            self.conversation
                .push(ConversationItem::assistant(std::mem::take(
                    &mut self.current_agent_text,
                )));
            self.has_pending_agent = false;
        }
    }
}

/// Progressive post-checkpoint turn: unmarked users count until the first
/// marker in the slice; after that only marked users count.
fn counts_as_replay_turn_progressive(item: &ConversationItem, seen_marker: &mut bool) -> bool {
    let ConversationItem::User(u) = item else {
        return false;
    };
    if u.prompt_index.is_some() {
        *seen_marker = true;
        true
    } else {
        !*seen_marker
    }
}

/// Read and validate one checkpoint file. The error kind tells the caller
/// why: `NotFound`, `InvalidData` (corrupt), or `Unsupported` (schema).
fn read_checkpoint_file(path: &Path) -> io::Result<CompactionCheckpointFile> {
    let bytes = std::fs::read(path)?;
    let file: CompactionCheckpointFile = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if file.schema_version > 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported checkpoint schema version {}", file.schema_version),
        ));
    }
    Ok(file)
}

/// The error a rewind shows when the base checkpoint for its target is
/// unreadable. The TUI shows one truncated line, so the prompt range and the
/// advice come first and the path comes last.
fn unreadable_checkpoint_error(e: &io::Error, info: &CompactionCheckpointInfo) -> io::Error {
    let word = match e.kind() {
        io::ErrorKind::NotFound => "missing",
        io::ErrorKind::InvalidData => "corrupt",
        io::ErrorKind::Unsupported => "unsupported",
        _ => "unreadable",
    };
    let at = info.prompt_index_at_compaction;
    io::Error::new(
        e.kind(),
        format!(
            "checkpoint for prompts #{at} onward is {word}. Pick a prompt before #{at} or at or \
             after the next compaction. ({})",
            info.checkpoint_file
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::notification::{
        AutoContinueInfo, CompactionCheckpointFile, CompactionCheckpointInfo,
        SessionNotification as XaiNotification, SessionUpdate as XaiSessionUpdate,
    };
    use agent_client_protocol as acp;
    use tempfile::TempDir;

    fn make_user_update(session_id: &str, text: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_string()),
            ))),
        )))
    }

    fn make_user_update_pi(session_id: &str, text: &str, prompt_index: usize) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    text.to_string(),
                )))
                .meta(
                    serde_json::json!({ "promptIndex": prompt_index })
                        .as_object()
                        .cloned(),
                ),
            ),
        )))
    }

    #[test]
    fn test_replay_consecutive_prompts_no_agent_between_are_distinct_turns() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_user_update_pi("s1", "P1", 1),
            make_user_update_pi("s1", "P2", 2),
            make_user_update_pi("s1", "P3", 3),
            make_user_update_pi("s1", "P4", 4),
            make_user_update_pi("s1", "P5", 5),
        ];
        let result = replay_updates(&updates, tmp.path(), 3);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(
            user_msgs,
            vec!["P0", "P1", "P2"],
            "consecutive cancelled-turn prompts must be distinct turns and truncate correctly"
        );
        assert_eq!(result.prompt_index_reached, 3);
    }

    fn make_agent_update(session_id: &str, text: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_string()),
            ))),
        )))
    }

    fn make_host_turn_update(session_id: &str, text: &str, user: bool) -> SessionUpdate {
        let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        )))
        .meta(serde_json::json!({ "hostTurn": true }).as_object().cloned());
        let update = if user {
            acp::SessionUpdate::UserMessageChunk(chunk)
        } else {
            acp::SessionUpdate::AgentMessageChunk(chunk)
        };
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            update,
        )))
    }

    #[test]
    fn test_replay_suppresses_full_host_turn_and_flushes_preceding_agent() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "real user", 0),
            make_agent_update("s1", "real assistant"),
            make_host_turn_update("s1", "/workflows", true),
            make_host_turn_update("s1", "host-only slash output", false),
            make_user_update_pi("s1", "next real user", 1),
            make_agent_update("s1", "next real assistant"),
        ];

        let result = replay_updates(&updates, tmp.path(), 2);
        let texts: Vec<_> = result
            .conversation
            .iter()
            .map(ConversationItem::text_content)
            .collect();
        assert_eq!(
            texts,
            vec![
                "real user",
                "real assistant",
                "next real user",
                "next real assistant"
            ]
        );
        assert_eq!(result.prompt_index_reached, 2);
    }

    fn make_rewind_marker(target: usize) -> SessionUpdate {
        SessionUpdate::Xai(Box::new(XaiNotification {
            session_id: acp::SessionId::new("test"),
            update: XaiSessionUpdate::RewindMarker {
                target_prompt_index: target,
                created_at: "2024-01-01T00:00:00Z".to_string(),
            },
            meta: None,
        }))
    }

    fn make_checkpoint(
        checkpoint_id: &str,
        prompt_index_at_compaction: usize,
        auto_continue: Option<AutoContinueInfo>,
    ) -> SessionUpdate {
        SessionUpdate::Xai(Box::new(XaiNotification {
            session_id: acp::SessionId::new("test"),
            update: XaiSessionUpdate::CompactionCheckpoint(Box::new(CompactionCheckpointInfo {
                checkpoint_id: checkpoint_id.to_string(),
                prompt_index_at_compaction,
                checkpoint_file: format!("compaction_checkpoints/{checkpoint_id}.json"),
                auto_continue,
                schema_version: 1,
                created_at: "2024-01-01T00:00:00Z".to_string(),
            })),
            meta: None,
        }))
    }

    fn write_checkpoint_file(
        session_dir: &Path,
        checkpoint_id: &str,
        prompt_index_at_compaction: usize,
        compacted_history: Vec<ConversationItem>,
    ) {
        let dir = session_dir.join("compaction_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        let file = CompactionCheckpointFile {
            checkpoint_id: checkpoint_id.to_string(),
            prompt_index_at_compaction,
            compacted_history,
            schema_version: 1,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            original_user_info: None,
            reread_file_paths: vec![],
        };
        let bytes = serde_json::to_vec_pretty(&file).unwrap();
        std::fs::write(dir.join(format!("{checkpoint_id}.json")), bytes).unwrap();
    }

    /// Helper: write a sequence of updates to a JSONL file and replay to a target.
    fn replay_updates(
        updates: &[SessionUpdate],
        session_dir: &Path,
        target: usize,
    ) -> ReplayResult {
        let updates_path = session_dir.join("updates.jsonl");
        let mut content = Vec::new();
        for u in updates {
            let envelope = crate::session::storage::SessionUpdateEnvelope::from_update(u).unwrap();
            let mut line = serde_json::to_vec(&envelope).unwrap();
            line.push(b'\n');
            content.extend(line);
        }
        std::fs::write(&updates_path, content).unwrap();
        replay_to_prompt(&updates_path, session_dir, target).unwrap()
    }

    fn try_replay_updates(
        updates: &[SessionUpdate],
        session_dir: &Path,
        target: usize,
    ) -> io::Result<ReplayResult> {
        let updates_path = session_dir.join("updates.jsonl");
        let mut content = Vec::new();
        for u in updates {
            let envelope = crate::session::storage::SessionUpdateEnvelope::from_update(u).unwrap();
            let mut line = serde_json::to_vec(&envelope).unwrap();
            line.push(b'\n');
            content.extend(line);
        }
        std::fs::write(&updates_path, content).unwrap();
        replay_to_prompt(&updates_path, session_dir, target)
    }

    /// Two compactions: ckpt1 at prompt 2, ckpt2 at prompt 3.
    fn two_compactions_updates() -> Vec<SessionUpdate> {
        vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
            make_user_update("s1", "P4"),
            make_agent_update("s1", "R4"),
        ]
    }

    fn summary_history(summary: &str) -> Vec<ConversationItem> {
        vec![ConversationItem::system("sys"), ConversationItem::user(summary)]
    }

    /// The 30-day sweep can delete an older checkpoint. A rewind whose base
    /// is a later checkpoint must not need it.
    #[test]
    fn test_replay_missing_superseded_checkpoint_still_rewinds() {
        let tmp = TempDir::new().unwrap();
        write_checkpoint_file(tmp.path(), "ckpt2", 3, summary_history("summary2"));

        let result = try_replay_updates(&two_compactions_updates(), tmp.path(), 4)
            .expect("ckpt1 is superseded by ckpt2, so its absence must not matter");

        let texts: Vec<String> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["sys", "summary2", "P3", "R3"]);
        assert_eq!(result.prompt_index_reached, 4);
        assert_eq!(result.last_compaction_prompt_index, Some(3));
    }

    #[test]
    fn test_replay_missing_base_checkpoint_errors() {
        let tmp = TempDir::new().unwrap();
        write_checkpoint_file(tmp.path(), "ckpt1", 2, summary_history("summary1"));

        let err = try_replay_updates(&two_compactions_updates(), tmp.path(), 4)
            .expect_err("the base checkpoint for prompt 4 is ckpt2, which is missing");

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let message = err.to_string();
        assert!(
            message.starts_with("checkpoint for prompts #3 onward is missing."),
            "{message}"
        );
        assert!(message.contains("compaction_checkpoints/ckpt2.json"), "{message}");
    }

    #[test]
    fn test_replay_missing_later_checkpoint_pre_compaction_target_succeeds() {
        let tmp = TempDir::new().unwrap();

        let result = try_replay_updates(&two_compactions_updates(), tmp.path(), 1)
            .expect("checkpoints after the target only supply user_info");

        let texts: Vec<String> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["P0", "R0"]);
        assert_eq!(result.last_compaction_prompt_index, None);
    }

    #[test]
    fn test_replay_rewind_marker_discards_missing_checkpoint() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("gone", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_rewind_marker(1),
            make_user_update("s1", "P1b"),
            make_agent_update("s1", "R1b"),
            make_user_update("s1", "P2b"),
            make_agent_update("s1", "R2b"),
        ];

        let result = try_replay_updates(&updates, tmp.path(), 2)
            .expect("the marker discards the timeline of the missing checkpoint");

        let texts: Vec<String> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["P0", "R0", "P1b", "R1b"]);
    }

    #[test]
    fn test_replay_unmarked_phantom_between_marked_prompts() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_agent_update("s1", "A0"),
            make_user_update("s1", "!pwd phantom"),
            make_agent_update("s1", "bash"),
            make_user_update_pi("s1", "P1", 1),
            make_agent_update("s1", "A1"),
            make_user_update_pi("s1", "P2", 2),
            make_agent_update("s1", "A2"),
        ];
        let result = replay_updates(&updates, tmp.path(), 2);
        let real: Vec<_> = result
            .conversation
            .iter()
            .filter_map(|c| match c {
                ConversationItem::User(u) if u.prompt_index.is_some() => Some(c.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(real, vec!["P0", "P1"]);
        assert!(
            result.conversation.iter().any(|c| {
                matches!(
                    c,
                    ConversationItem::User(u)
                        if u.prompt_index.is_none() && c.text_content().contains("pwd")
                )
            }),
            "phantom text kept for context"
        );
        assert_eq!(result.prompt_index_reached, 2);
    }

    #[test]
    fn test_replay_simple_no_compaction() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update("s1", "hello"),
            make_agent_update("s1", "hi there"),
            make_user_update("s1", "fix the bug"),
            make_agent_update("s1", "done"),
            make_user_update("s1", "add tests"),
            make_agent_update("s1", "tests added"),
        ];

        // Replay to prompt 1: keep prompts 0..0 = just "hello"
        let result = replay_updates(&updates, tmp.path(), 1);
        assert_eq!(result.prompt_index_reached, 1);
        assert_eq!(result.conversation.len(), 2);
        assert_eq!(result.conversation[0].text_content(), "hello");
        assert_eq!(result.conversation[1].text_content(), "hi there");
    }

    #[test]
    fn test_replay_with_rewind_marker() {
        let tmp = TempDir::new().unwrap();
        // P0, P1, P2, rewind(1) removes P1+P2, P1'
        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_rewind_marker(1), // keep P0 only
            make_user_update("s1", "P1_prime"),
            make_agent_update("s1", "R1_prime"),
        ];

        // Replay to prompt 2: keep prompts 0..1 = P0, P1'
        let result = replay_updates(&updates, tmp.path(), 2);
        // After rewind(1): P0 kept, P1+P2 discarded
        // P1_prime added as prompt 1 → [P0, R0, P1_prime, R1_prime]
        assert_eq!(result.conversation.len(), 4);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(user_msgs, vec!["P0", "P1_prime"]);
    }

    #[test]
    fn test_replay_pre_compaction_target() {
        let tmp = TempDir::new().unwrap();

        // P0, P1, checkpoint(at=2), P2
        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted summary"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
        ];

        // Replay to prompt 1 (pre-compaction) — should IGNORE the checkpoint
        // Keep prompts 0..0 = just P0
        let result = replay_updates(&updates, tmp.path(), 1);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(user_msgs, vec!["P0"]);
    }

    #[test]
    fn test_replay_post_compaction_target() {
        let tmp = TempDir::new().unwrap();

        // Checkpoint replaces conversation at prompt 2
        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted summary"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ];

        // Replay to prompt 3 (post-compaction): keep prompts 0..2
        // Checkpoint blob + P2 only (P3 removed)
        let result = replay_updates(&updates, tmp.path(), 3);
        assert_eq!(result.conversation.len(), 4);
        assert_eq!(result.conversation[0].text_content(), "sys");
        assert_eq!(result.conversation[1].text_content(), "compacted summary");
        assert_eq!(result.conversation[2].text_content(), "P2");
        assert_eq!(result.conversation[3].text_content(), "R2");
        assert_eq!(result.prompt_index_reached, 3);
    }

    /// A checkpoint written before this binary's validation (or before the
    /// API tightened its validators) can carry an unsendable image; the
    /// splice must heal it like the jsonl loader does, or a cross-compaction
    /// rewind re-poisons a healed session.
    #[test]
    fn test_replay_checkpoint_strips_invalid_images() {
        use base64::Engine as _;
        let tmp = TempDir::new().unwrap();

        // 16×16 icon: below the API's 512-total-pixel floor.
        let mut png = Vec::new();
        image::ImageBuffer::from_pixel(16, 16, image::Rgba([9u8, 9, 9, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let mut poisoned = ConversationItem::user("look at this icon");
        poisoned.add_image(url);

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            1,
            vec![ConversationItem::system("sys"), poisoned],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_checkpoint("ckpt1", 1, None),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
        ];

        let result = replay_updates(&updates, tmp.path(), 2);
        let ConversationItem::User(u) = &result.conversation[1] else {
            panic!("expected user item from checkpoint");
        };
        assert!(
            u.content.iter().all(|p| match p {
                crate::sampling::ContentPart::Image { url } => !url.starts_with("data:"),
                _ => true,
            }),
            "below-floor image must be stripped from the checkpoint splice"
        );
    }

    /// Auto-continue prompt is synthetic (not a real user prompt) so it must
    /// NOT increment prompt_counter. It's appended to the conversation for
    /// context but the next real prompt still gets the expected index.
    #[test]
    fn test_replay_checkpoint_with_auto_continue() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint(
                "ckpt1",
                2,
                Some(AutoContinueInfo {
                    prompt_text: "Continue working".to_string(),
                }),
            ),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
        ];

        // Replay to prompt 3: keep prompts 0..2 = checkpoint blob + auto-continue + P2
        let result = replay_updates(&updates, tmp.path(), 3);
        // checkpoint sets counter to 2, auto-continue doesn't increment,
        // P2 increments to 3 → prompt_index_reached = 3
        assert_eq!(result.conversation.len(), 5);
        assert_eq!(result.conversation[0].text_content(), "sys");
        assert_eq!(result.conversation[1].text_content(), "compacted");
        assert_eq!(result.conversation[2].text_content(), "Continue working");
        assert_eq!(result.conversation[3].text_content(), "P2");
        assert_eq!(result.conversation[4].text_content(), "R2");
        assert_eq!(result.prompt_index_reached, 3);
    }

    #[test]
    fn test_find_latest_checkpoint_none() {
        let tmp = TempDir::new().unwrap();
        let updates_path = tmp.path().join("updates.jsonl");
        std::fs::write(&updates_path, "").unwrap();

        let result = find_latest_compaction_checkpoint(&updates_path).unwrap();
        assert!(result.is_none());
    }

    /// Scenario H: rewind marker after a loaded checkpoint.
    /// checkpoint(at=2), P2, P3, RewindMarker(2), P2'
    /// Replaying to prompt 2 should give checkpoint + P2' (not P2 or P3).
    #[test]
    fn test_replay_rewind_marker_after_checkpoint() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted summary"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
            make_rewind_marker(2),
            make_user_update("s1", "P2_prime"),
            make_agent_update("s1", "R2_prime"),
        ];

        // Replay to prompt 3: keep 0..2 = checkpoint + P2' (after the rewind marker)
        let result = replay_updates(&updates, tmp.path(), 3);

        // The checkpoint blob has 2 items (system + user summary).
        // After the rewind marker discards P2+P3, P2' is added.
        // Result: [sys, summary, P2_prime, R2_prime]
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();

        // The compacted summary is a synthetic User msg inside the checkpoint blob.
        // P2_prime is the real user msg appended after.
        assert!(
            user_msgs.contains(&"P2_prime".to_string()),
            "Should contain P2_prime, got: {:?}",
            user_msgs
        );
        assert!(
            !user_msgs.contains(&"P2".to_string()),
            "Should NOT contain old P2, got: {:?}",
            user_msgs
        );
        assert!(
            !user_msgs.contains(&"P3".to_string()),
            "Should NOT contain P3, got: {:?}",
            user_msgs
        );
    }

    /// Scenario E: multiple compactions, rewind to before the first.
    /// P0, P1 → checkpoint#1(at=2) → P2 → checkpoint#2(at=3) → P3
    /// Rewind to P1 should ignore both checkpoints.
    #[test]
    fn test_replay_multiple_compactions_rewind_to_before_first() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary1"),
            ],
        );
        write_checkpoint_file(
            tmp.path(),
            "ckpt2",
            3,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary2"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ];

        // Rewind to P1 — both checkpoints should be ignored
        // Keep prompts 0..0 = just P0
        let result = replay_updates(&updates, tmp.path(), 1);
        assert_eq!(result.conversation.len(), 2);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(user_msgs, vec!["P0"]);
    }

    /// Scenario E variant: rewind to between two compactions.
    /// Should use checkpoint#1 and replay P2.
    #[test]
    fn test_replay_multiple_compactions_rewind_between() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary1"),
            ],
        );
        write_checkpoint_file(
            tmp.path(),
            "ckpt2",
            3,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary2"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ];

        // Replay to prompt 3: keep prompts 0..2 via ckpt1
        // ckpt1 loaded (target 3 >= 2), ckpt2 also loaded (target 3 >= 3).
        // ckpt2 replaces ckpt1. Then P3 is the first post-ckpt2 prompt.
        // Keep 3 - 3 = 0 post-ckpt2 prompts → just ckpt2 blob.
        let result = replay_updates(&updates, tmp.path(), 3);
        assert_eq!(result.conversation.len(), 2);
        assert_eq!(result.conversation[0].text_content(), "sys");
        assert_eq!(result.conversation[1].text_content(), "summary2");
    }
}
