//! Session fork/copy for the JSONL adapter.
//!
//! The `updates.jsonl` transcript is unbounded, so the copy streams it line by
//! line: peak memory tracks a single capped line, plus one small per-line
//! record when a prompt cut is requested. Chat history stays materialized: its
//! transforms need random access and the compacted history is bounded by the
//! context window.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::ops::ControlFlow;
use std::path::Path;

use agent_client_protocol as acp;

use crate::sampling::{
    ConversationItem, conversation_truncate_for_prompt, fork_filter_chat,
    transform_conversation_cwd,
};
use crate::session::info::Info;
use crate::session::persistence::{CHAT_FORMAT_VERSION, Summary};
use crate::session::storage::jsonl::{JsonlStorageAdapter, transform_session_id_in_update};
use crate::session::storage::{
    CopySessionOptions, CopySessionResult, RewindStep, SessionUpdate, SessionUpdateEnvelope,
    filter_rewind_by, rewind_step_for_line, truncate_for_prompt_by,
};

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;

/// Copy only verified, referenced source assets and rewrite runtime envelopes.
/// Query literals and paths outside the source session's direct assets stay unchanged.
struct ForkImageAssets {
    source: std::path::PathBuf,
    target: std::path::PathBuf,
    copied: BTreeMap<String, String>,
}

/// Rebuild runtime attachment blocks from copied paths, never touching query
/// literals or already-quoted background text. An empty map strips metadata.
fn selected_image_metadata(text: &str, copied: &BTreeMap<String, String>) -> String {
    use xai_chat_state::compaction_image_context::{
        parse_image_files_paths, render_image_files_block,
    };
    let boundary = ["<user_query>", "<background_context>"]
        .iter()
        .filter_map(|tag| text.find(tag))
        .min()
        .unwrap_or(text.len());
    let mut prefix = &text[..boundary];
    let mut result = String::new();
    while let Some(start) = prefix.find("<image_files>") {
        // Legacy unwrapped messages have runtime metadata only at the front.
        // Once ordinary query text begins, later blocks are literal examples.
        if boundary == text.len() && !prefix[..start].trim().is_empty() {
            break;
        }
        let Some(end) = prefix[start..].find("</image_files>") else {
            break;
        };
        let end = start + end + "</image_files>".len();
        result.push_str(&prefix[..start]);
        let selected: Vec<_> = parse_image_files_paths(&prefix[start..end])
            .into_iter()
            .filter_map(|path| copied.get(&path).cloned())
            .collect();
        if let Some(block) = render_image_files_block(&selected) {
            result.push_str(&block);
        }
        prefix = &prefix[end..];
    }
    result.push_str(prefix);
    result.push_str(&text[boundary..]);
    result
}

impl ForkImageAssets {
    fn copy_asset(&mut self, path: &str) -> io::Result<Option<String>> {
        if let Some(copied) = self.copied.get(path) {
            return Ok(Some(copied.clone()));
        }
        let candidate = Path::new(path);
        if !crate::session::helpers::session_compact::is_direct_session_asset_path(
            path,
            &self.source,
        ) || !std::fs::symlink_metadata(&self.source).is_ok_and(|m| m.is_dir())
            || !std::fs::symlink_metadata(candidate).is_ok_and(|m| m.is_file())
        {
            return Ok(None);
        }
        if self.source == self.target {
            self.copied.insert(path.to_owned(), path.to_owned());
            return Ok(Some(path.to_owned()));
        }
        let mut input_options = std::fs::OpenOptions::new();
        input_options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            input_options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut input = input_options.open(candidate)?;
        if !input.metadata()?.is_file() {
            return Ok(None);
        }
        match std::fs::symlink_metadata(&self.target) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fork image assets destination is not a regular directory",
                ));
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        crate::util::grok_home::create_dir_all_owner_only(&self.target)?;
        if !std::fs::symlink_metadata(&self.target)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fork image assets destination is not a regular directory",
            ));
        }
        let extension = candidate
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("image");
        let destination = self
            .target
            .join(format!("image-{}.{}", uuid::Uuid::new_v4(), extension));
        let mut output_options = std::fs::OpenOptions::new();
        output_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            output_options.mode(0o600);
        }
        let mut output = output_options.open(&destination)?;
        if let Err(error) = io::copy(&mut input, &mut output).and_then(|_| output.flush()) {
            let _ = std::fs::remove_file(&destination);
            return Err(error);
        }
        let replacement = destination.to_string_lossy().into_owned();
        self.copied.insert(path.to_owned(), replacement.clone());
        Ok(Some(replacement))
    }

    fn rewrite_text(&mut self, text: &str) -> io::Result<String> {
        use xai_chat_state::compaction_image_context::{
            parse_image_files_paths, rewrite_image_files_paths,
        };
        for path in parse_image_files_paths(text) {
            self.copy_asset(&path)?;
        }
        Ok(rewrite_image_files_paths(text, &self.copied))
    }

    fn copy_newest_paths(&mut self, paths: Vec<String>) -> io::Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut copied = Vec::new();
        for path in paths.into_iter().rev() {
            if seen.insert(path.clone())
                && let Some(path) = self.copy_asset(&path)?
            {
                copied.push(path);
                if copied.len()
                    == crate::session::helpers::session_compact::MAX_COMPACTION_IMAGE_PATHS
                {
                    break;
                }
            }
        }
        copied.reverse();
        Ok(copied)
    }

    fn carry_image_paths(
        &mut self,
        conversation: &mut Vec<ConversationItem>,
        paths: Vec<String>,
    ) -> io::Result<()> {
        let copied = self.copy_newest_paths(paths)?;
        let observed: BTreeSet<String> = conversation
            .iter()
            .flat_map(|item| {
                xai_chat_state::compaction_image_context::parse_image_files_paths(
                    &item.text_content(),
                )
            })
            .collect();
        let missing: Vec<_> = copied
            .into_iter()
            .filter(|path| !observed.contains(path))
            .collect();
        if let Some(block) =
            xai_chat_state::compaction_image_context::render_image_files_block(&missing)
        {
            let position = conversation
                .iter()
                .take_while(|item| matches!(item, ConversationItem::System(_)))
                .count();
            conversation.insert(position, ConversationItem::user_meta(block));
        }
        Ok(())
    }

    fn rewrite_conversation(&mut self, conversation: &mut [ConversationItem]) -> io::Result<bool> {
        let mut changed = false;
        for item in conversation {
            if let ConversationItem::User(user) = item {
                for part in &mut user.content {
                    if let crate::sampling::ContentPart::Text { text } = part {
                        let rewritten = self.rewrite_text(text)?;
                        if rewritten != text.as_ref() {
                            *text = rewritten.into();
                            changed = true;
                        }
                    }
                }
            }
        }
        Ok(changed)
    }
}

fn is_orchestration_projection_update(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::Xai(notification)
            if matches!(
                &notification.update,
                crate::extensions::notification::SessionUpdate::WorkflowUpdated { .. }
                    | crate::extensions::notification::SessionUpdate::GoalUpdated { .. }
            )
    )
}

/// Updates written plus the `compaction_checkpoints/{uuid}.json` files the
/// surviving records reference, collected in the same pass.
#[derive(Default)]
struct CopiedUpdates {
    count: usize,
    checkpoint_files: BTreeSet<String>,
}

/// Longest `updates.jsonl` line the copy will buffer; anything past it is
/// corruption (e.g. a tail that lost its newlines) and is discarded without
/// being buffered. Discarded lines consume no index in either pass, unlike
/// torn lines, which classify as [`RewindStep::Other`] and end a user run.
const MAX_UPDATE_LINE_BYTES: usize = 64 * 1024 * 1024;

/// [`for_each_jsonl_line_capped`] with the production cap.
fn for_each_jsonl_line<R: BufRead>(
    reader: R,
    f: impl FnMut(usize, &[u8]) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    for_each_jsonl_line_capped(reader, MAX_UPDATE_LINE_BYTES, f)
}

/// Invoke `f` with the index and bytes of each non-empty line, reusing one
/// capped line buffer. Lines over `cap` content bytes are discarded without
/// being buffered whole and consume no index. `f` returns `Break` to stop
/// early. Raw bytes rather than the typed `UpdatesIterator`: classification
/// must tolerate non-UTF-8 lines, and both copy passes need identical line
/// indexes.
fn for_each_jsonl_line_capped<R: BufRead>(
    mut reader: R,
    cap: usize,
    mut f: impl FnMut(usize, &[u8]) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    let mut buf = Vec::new();
    let mut index = 0;
    let mut discarded = 0usize;
    let result = loop {
        buf.clear();
        let n = reader
            .by_ref()
            .take(cap as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            break Ok(());
        }
        if buf.len() > cap && buf.last() != Some(&b'\n') {
            discarded += 1;
            if discarded == 1 {
                tracing::warn!(
                    max_bytes = cap,
                    "discarding over-long updates.jsonl line during fork copy"
                );
            }
            // Drain the remainder of the line without retaining it.
            loop {
                buf.clear();
                let n = reader
                    .by_ref()
                    .take(cap as u64)
                    .read_until(b'\n', &mut buf)?;
                if n == 0 || buf.last() == Some(&b'\n') {
                    break;
                }
            }
            continue;
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        if f(index, line)?.is_break() {
            break Ok(());
        }
        index += 1;
    };
    if discarded > 1 {
        tracing::warn!(
            discarded,
            max_bytes = cap,
            "discarded over-long updates.jsonl lines during fork copy"
        );
    }
    result
}

/// Indexes (in non-empty-line order) of the source lines that survive rewind
/// filtering and the `target_prompt_index` cut, holding one classification per
/// line instead of the lines. As in replay, an unparseable line classifies as
/// [`RewindStep::Other`] (ending a user run) and is skipped later at parse.
fn surviving_line_indexes<R: BufRead>(
    reader: R,
    target_prompt_index: usize,
) -> io::Result<Vec<usize>> {
    struct LineRecord {
        index: usize,
        step: RewindStep,
    }
    let mut records = Vec::new();
    for_each_jsonl_line(reader, |index, line| {
        let step = std::str::from_utf8(line).map_or(RewindStep::Other, rewind_step_for_line);
        records.push(LineRecord { index, step });
        Ok(ControlFlow::Continue(()))
    })?;
    let mut records = filter_rewind_by(records, |record| record.step);
    let keep = truncate_for_prompt_by(&records, target_prompt_index, |record| record.step);
    records.truncate(keep);
    Ok(records.into_iter().map(|record| record.index).collect())
}

/// Streaming writer for the fork target's `updates.jsonl`. Corruption-tolerant
/// like the load path: a torn or undecodable line is skipped with a warning
/// instead of failing the fork.
struct UpdateLineWriter<'a> {
    writer: BufWriter<std::fs::File>,
    source: &'a Path,
    target_session_id: &'a acp::SessionId,
    copied: CopiedUpdates,
    skipped_lines: usize,
    assets: &'a mut ForkImageAssets,
}

impl<'a> UpdateLineWriter<'a> {
    fn try_new(
        target: &Path,
        source: &'a Path,
        target_session_id: &'a acp::SessionId,
        assets: &'a mut ForkImageAssets,
    ) -> io::Result<Self> {
        Ok(Self {
            writer: BufWriter::new(std::fs::File::create(target)?),
            source,
            target_session_id,
            copied: CopiedUpdates::default(),
            skipped_lines: 0,
            assets,
        })
    }

    fn copy_line(&mut self, line: &[u8]) -> io::Result<()> {
        let mut update = match std::str::from_utf8(line).map(SessionUpdateEnvelope::from_str) {
            Ok(Ok(update)) => update,
            Ok(Err(error)) => {
                self.skip_torn_line(&error);
                return Ok(());
            }
            Err(error) => {
                self.skip_torn_line(&error);
                return Ok(());
            }
        };
        if let SessionUpdate::Acp(notification) = &mut update
            && let acp::SessionUpdate::UserMessageChunk(chunk) = &mut notification.update
            // Ordinary turns persist the user's raw prompt with a prompt index;
            // only synthetic interjections carry an attachment envelope here.
            && chunk.meta.as_ref().is_none_or(|meta| !meta.contains_key("promptIndex"))
            && let acp::ContentBlock::Text(text) = &mut chunk.content
            && text.text.find("<user_query>").is_some_and(|query| {
                text.text[..query].contains(xai_interjection_core::INTERJECTION_NOTE)
            })
        {
            text.text = self.assets.rewrite_text(&text.text)?;
        }
        if is_orchestration_projection_update(&update) {
            return Ok(());
        }
        if let SessionUpdate::Xai(notification) = &update
            && let crate::extensions::notification::SessionUpdate::CompactionCheckpoint(info) =
                &notification.update
        {
            self.copied
                .checkpoint_files
                .insert(info.checkpoint_file.clone());
        }
        let update = transform_session_id_in_update(update, self.target_session_id);
        let envelope = SessionUpdateEnvelope::from_update(&update).map_err(invalid_data)?;
        serde_json::to_writer(&mut self.writer, &envelope).map_err(invalid_data)?;
        self.writer.write_all(b"\n")?;
        self.copied.count += 1;
        Ok(())
    }

    fn skip_torn_line(&mut self, error: &dyn std::fmt::Display) {
        self.skipped_lines += 1;
        if self.skipped_lines == 1 {
            tracing::warn!(
                error = %error,
                path = %self.source.display(),
                "skipping unparseable updates.jsonl line during fork copy (torn append?)"
            );
        }
    }

    fn finish(mut self) -> io::Result<CopiedUpdates> {
        // The first skipped line already warned with its parse error.
        if self.skipped_lines > 1 {
            tracing::warn!(
                skipped = self.skipped_lines,
                copied = self.copied.count,
                path = %self.source.display(),
                "skipped unparseable session update lines during fork copy"
            );
        }
        self.writer.flush()?;
        Ok(self.copied)
    }
}

/// Copy `source` (an `updates.jsonl`) to `target` without materializing it.
/// With a `target_prompt_index`, pass one computes the surviving line set and
/// pass two writes exactly those lines; without one, every line streams
/// through, preserving rewind markers and dead branches. Both passes read one
/// pinned, rewound file handle, so their line indexes cannot skew under a
/// concurrent rename; `updates.jsonl` is append-only by contract, so lines
/// appended after pass one land past every survivor index.
fn copy_updates_streaming(
    source: &Path,
    target: &Path,
    target_session_id: &acp::SessionId,
    target_prompt_index: Option<usize>,
    assets: &mut ForkImageAssets,
) -> io::Result<CopiedUpdates> {
    let mut writer = UpdateLineWriter::try_new(target, source, target_session_id, assets)?;
    let mut file = match std::fs::File::open(source) {
        Ok(file) => file,
        // A missing source is an empty transcript; still write the target.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return writer.finish(),
        Err(error) => return Err(error),
    };
    match target_prompt_index {
        None => {
            for_each_jsonl_line(BufReader::new(file), |_, line| {
                writer.copy_line(line)?;
                Ok(ControlFlow::Continue(()))
            })?;
        }
        Some(target_idx) => {
            let survivors = surviving_line_indexes(BufReader::new(&mut file), target_idx)?;
            file.seek(io::SeekFrom::Start(0))?;
            let mut survivors = survivors.into_iter().peekable();
            for_each_jsonl_line(BufReader::new(file), |index, line| {
                if survivors.next_if_eq(&index).is_some() {
                    writer.copy_line(line)?;
                }
                Ok(if survivors.peek().is_none() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })?;
        }
    }
    writer.finish()
}

impl JsonlStorageAdapter {
    /// A live parent can expose an assistant before its evidence writer has
    /// recovered. Never accept a fork whose inherited hashes are unavailable
    /// or whose copied content does not match the hash.
    fn validate_request_evidence_sync(
        &self,
        info: &Info,
        conversation: &[ConversationItem],
    ) -> io::Result<()> {
        use sha2::{Digest, Sha256};
        let mut references = BTreeSet::new();
        for item in conversation {
            if let ConversationItem::Assistant(assistant) = item {
                if let Some(sha) = &assistant.tools_sha256 {
                    references.insert(("request_tools", sha.as_str(), "json"));
                }
                if let Some(sha) = &assistant.system_sha256 {
                    references.insert(("system_prompts", sha.as_str(), "txt"));
                }
            }
        }
        let directory = self.session_dir(info);
        for (kind, sha, extension) in references {
            if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid inherited {kind} hash: {sha}"),
                ));
            }
            let path = directory.join(kind).join(format!("{sha}.{extension}"));
            let bytes = std::fs::read(&path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "fork request evidence unavailable at {}: {error}",
                        path.display()
                    ),
                )
            })?;
            if format!("{:x}", Sha256::digest(&bytes)) != sha {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fork request evidence hash mismatch at {}", path.display()),
                ));
            }
        }
        Ok(())
    }

    /// Preserve immutable request evidence, including calls archived by compaction.
    /// The files describe original requests, so never apply a cwd rewrite to them.
    fn copy_request_evidence_sync(&self, source_info: &Info, target_info: &Info) -> io::Result<()> {
        let source_dir = self.session_dir(source_info);
        let target_dir = self.session_dir(target_info);
        if source_dir == target_dir {
            return Ok(());
        }
        for directory in ["request_tools", "system_prompts"] {
            let entries = match std::fs::read_dir(source_dir.join(directory)) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let destination = target_dir.join(directory);
            std::fs::create_dir_all(&destination)?;
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    std::fs::copy(entry.path(), destination.join(entry.file_name()))?;
                }
            }
        }
        Ok(())
    }

    /// Attachment paths are carried separately. Strip their runtime envelopes
    /// before rendering a normalized parent's transcript as quoted background.
    pub(crate) fn strip_fork_image_metadata(conversation: &mut [ConversationItem]) {
        for item in conversation {
            if let ConversationItem::User(user) = item {
                for part in &mut user.content {
                    if let crate::sampling::ContentPart::Text { text } = part {
                        *text = selected_image_metadata(text, &BTreeMap::new()).into();
                    }
                }
            }
        }
    }
    /// Rebase live child bootstrap attachments and copy request evidence without
    /// copying a disk transcript, including live forks with no images.
    pub(crate) async fn rebase_fork_image_assets(
        &self,
        source_info: &Info,
        target_info: &Info,
        mut conversation: Vec<ConversationItem>,
        image_paths: Vec<String>,
    ) -> io::Result<Vec<ConversationItem>> {
        let storage = self.clone();
        let source_info = source_info.clone();
        let target_info = target_info.clone();
        tokio::task::spawn_blocking(move || {
            let target_dir = storage.create_session_dir_owner_only(&target_info)?;
            storage.copy_request_evidence_sync(&source_info, &target_info)?;
            storage.validate_request_evidence_sync(&target_info, &conversation)?;
            let mut assets = ForkImageAssets {
                source: storage.session_dir(&source_info).join("assets"),
                target: target_dir.join("assets"),
                copied: BTreeMap::new(),
            };
            // Select verified newest paths before rewriting the in-memory transcript.
            let selected = assets.copy_newest_paths(image_paths)?;
            for item in &mut conversation {
                if let ConversationItem::User(user) = item {
                    for part in &mut user.content {
                        if let crate::sampling::ContentPart::Text { text } = part {
                            *text = selected_image_metadata(text, &assets.copied).into();
                        }
                    }
                }
            }
            let present: BTreeSet<_> = conversation
                .iter()
                .flat_map(|item| {
                    xai_chat_state::compaction_image_context::parse_image_files_paths(
                        &item.text_content(),
                    )
                })
                .collect();
            // Only add a note for metadata lost by background normalization/filtering.
            let missing: Vec<_> = selected
                .into_iter()
                .filter(|path| !present.contains(path))
                .collect();
            if let Some(block) =
                xai_chat_state::compaction_image_context::render_image_files_block(&missing)
            {
                let position = conversation
                    .iter()
                    .take_while(|item| matches!(item, ConversationItem::System(_)))
                    .count();
                conversation.insert(position, ConversationItem::user_meta(block));
            }
            Ok(conversation)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Fully synchronous implementation of `copy_session_data`, for use on a
    /// blocking thread; every caller reaches it through `spawn_blocking`.
    pub(crate) fn copy_session_data_sync(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: CopySessionOptions,
    ) -> io::Result<CopySessionResult> {
        // Canonical creator: the fork target chain is born owner-only.
        let target_dir = self.create_session_dir_owner_only(target_info)?;

        let mut assets = ForkImageAssets {
            source: self.session_dir(source_info).join("assets"),
            target: target_dir.join("assets"),
            copied: BTreeMap::new(),
        };

        let source_summary = self.read_summary_sync(source_info)?;
        let chat_format_version = source_summary.chat_format_version;

        let mut chat_to_copy: Vec<ConversationItem> =
            self.read_chat_history_sync(self.chat_file(source_info), chat_format_version)?;

        if let Some(target_idx) = options.target_prompt_index {
            // +1: the cut keeps the target prompt inclusive.
            let keep = conversation_truncate_for_prompt(&chat_to_copy, target_idx + 1);
            chat_to_copy.truncate(keep);
        }

        let filtered_image_paths = if options.fork_filter {
            // Carry only attachment metadata, including a newest image whose
            // incomplete turn is removed by the unchanged fork-safety filter.
            let paths = xai_chat_state::compaction_image_context::collect_attached_image_paths(
                &chat_to_copy,
                &[],
            );
            fork_filter_chat(&mut chat_to_copy);
            paths
        } else {
            Vec::new()
        };

        assets.rewrite_conversation(&mut chat_to_copy)?;
        if !filtered_image_paths.is_empty() {
            assets.carry_image_paths(&mut chat_to_copy, filtered_image_paths)?;
        }

        for target in [
            self.workflows_dir(target_info),
            self.goal_mode_state_file(target_info)
                .parent()
                .expect("goal state has a parent")
                .to_path_buf(),
        ] {
            match std::fs::remove_dir_all(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        // The child inherits everything below this boundary; compaction
        // preserves it.
        let inherited_prefix_len = if options.fork_filter {
            Some(chat_to_copy.len())
        } else {
            options.inherited_prefix_len
        };

        // Worktree forks skip the cwd rewrite: their display_cwd already
        // shows the model the original project path, and rewritten
        // conversation paths would contradict it.
        if !options.skip_cwd_transform && source_info.cwd != target_info.cwd {
            transform_conversation_cwd(&mut chat_to_copy, &source_info.cwd, &target_info.cwd);
            // Assets remain under session storage even when that storage is under
            // source cwd. The generic workspace rewrite must not relocate them.
            let restored_paths: BTreeMap<String, String> = assets
                .copied
                .values()
                .map(|path| {
                    (
                        path.replace(&source_info.cwd, &target_info.cwd),
                        path.clone(),
                    )
                })
                .collect();
            for item in &mut chat_to_copy {
                if let ConversationItem::User(user) = item {
                    for part in &mut user.content {
                        if let crate::sampling::ContentPart::Text { text } = part {
                            *text = xai_chat_state::compaction_image_context::rewrite_image_files_paths(
                                text, &restored_paths,
                            )
                            .into();
                        }
                    }
                }
            }
        }

        if options.strip_reasoning {
            chat_to_copy = xai_chat_state::compaction_utils::strip_reasoning_blocks(chat_to_copy);
        }

        self.copy_request_evidence_sync(source_info, target_info)?;
        self.validate_request_evidence_sync(target_info, &chat_to_copy)?;

        let num_chat_messages = chat_to_copy.len();
        let cwd_switch_bookkeeping_generation = chat_to_copy
            .iter()
            .filter_map(ConversationItem::working_directory_switch_generation)
            .max()
            .unwrap_or(0);

        // Release chat history before the (typically much larger) updates copy.
        {
            let mut writer = BufWriter::new(std::fs::File::create(self.chat_file(target_info))?);
            for item in &chat_to_copy {
                serde_json::to_writer(&mut writer, item).map_err(invalid_data)?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
        }
        drop(chat_to_copy);

        // A fork_filter copy (subagent context bootstrap) starts the child with
        // an empty replay transcript, so the source updates are never read.
        let copied_updates = if options.fork_filter {
            std::fs::write(self.updates_file(target_info), b"")?;
            CopiedUpdates::default()
        } else {
            copy_updates_streaming(
                &self.updates_file(source_info),
                &self.updates_file(target_info),
                &target_info.id,
                options.target_prompt_index,
                &mut assets,
            )?
        };
        let checkpoint_files = copied_updates.checkpoint_files;
        let num_messages = copied_updates.count;

        let target_summary = fork_summary(
            source_summary,
            target_info,
            &options,
            ForkCounters {
                num_messages,
                num_chat_messages,
                cwd_switch_bookkeeping_generation,
                inherited_prefix_len,
            },
        );
        let summary_bytes = serde_json::to_vec_pretty(&target_summary).map_err(invalid_data)?;
        std::fs::write(self.summary_file(target_info), summary_bytes)?;

        let plan_copied = copy_sidecar_file(
            options.copy_plan_state,
            &self.plan_file(source_info),
            &self.plan_file(target_info),
        )?;
        let signals_copied = copy_sidecar_file(
            options.copy_signals,
            &self.signals_file(source_info),
            &self.signals_file(target_info),
        )?;
        let plan_mode_state_copied = copy_sidecar_file(
            options.copy_plan_mode_state,
            &self.plan_mode_state_file(source_info),
            &self.plan_mode_state_file(target_info),
        )?;
        let tool_state_copied = copy_sidecar_file(
            options.copy_tool_state,
            &self.session_dir(source_info).join("tool_state.json"),
            &self.session_dir(target_info).join("tool_state.json"),
        )?;
        let announcement_state_copied = copy_sidecar_file(
            options.copy_announcement_state,
            &self.announcement_state_file(source_info),
            &self.announcement_state_file(target_info),
        )?;

        // Copied verbatim: the archive is immutable, so no cwd rewrite.
        let compaction_segments_copied = if options.copy_compaction_segments {
            let src_dir = self
                .session_dir(source_info)
                .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
            let mut copied = 0usize;
            if src_dir.is_dir() {
                let dst_dir = self
                    .session_dir(target_info)
                    .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
                std::fs::create_dir_all(&dst_dir)?;
                for entry in std::fs::read_dir(&src_dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), dst_dir.join(entry.file_name()))?;
                        copied += 1;
                    }
                }
            }
            copied
        } else {
            0
        };

        let compaction_checkpoints_copied = copy_referenced_checkpoints(
            &checkpoint_files,
            &self.session_dir(source_info),
            &target_dir,
            &source_info.id,
            &mut assets,
            |history| self.validate_request_evidence_sync(target_info, history),
        )?;

        Ok(CopySessionResult {
            chat_messages_copied: num_chat_messages,
            updates_copied: num_messages,
            plan_state_copied: plan_copied,
            plan_mode_state_copied,
            signals_copied,
            tool_state_copied,
            announcement_state_copied,
            compaction_segments_copied,
            compaction_checkpoints_copied,
        })
    }
}

/// Counters produced by this copy that feed the fork target's summary, named
/// so the same-typed counts cannot transpose.
struct ForkCounters {
    num_messages: usize,
    num_chat_messages: usize,
    cwd_switch_bookkeeping_generation: u64,
    inherited_prefix_len: Option<usize>,
}

/// Build the fork target's summary: counters from this copy, fork identity
/// from `options`, and per field either inheritance from the source or a
/// fresh-session reset.
fn fork_summary(
    source: Summary,
    target_info: &Info,
    options: &CopySessionOptions,
    counters: ForkCounters,
) -> Summary {
    Summary {
        info: target_info.clone(),
        cwd_generation: source.cwd_generation,
        previous_cwd: source.previous_cwd,
        pending_cwd_switch_reminder: None,
        cwd_switch_bookkeeping_generation: counters.cwd_switch_bookkeeping_generation,
        session_summary: source.session_summary,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        num_messages: counters.num_messages,
        num_chat_messages: counters.num_chat_messages,
        current_model_id: options
            .new_model_id
            .clone()
            .map(acp::ModelId::new)
            .unwrap_or(source.current_model_id),
        parent_session_id: options.parent_session_id.clone(),
        forked_at: Some(chrono::Utc::now()),
        collection_id: None,
        next_trace_turn: 0,
        chat_format_version: CHAT_FORMAT_VERSION,
        prompt_display_cwd: options.prompt_display_cwd.clone(),
        session_kind: Some(
            options
                .session_kind
                .clone()
                .unwrap_or_else(|| "fork".to_string()),
        ),
        fork_context_source: options.fork_context_source.clone(),
        fork_parent_prompt_id: options.fork_parent_prompt_id.clone(),
        inherited_prefix_len: counters.inherited_prefix_len,
        hidden: None,
        source_workspace_dir: options.source_workspace_dir.clone(),
        git_root_dir: None,
        git_remotes: Vec::new(),
        head_commit: source.head_commit,
        head_branch: source.head_branch,
        request_id: None,
        // Fresh local grok_home, not inherited from source: the fork lives on this machine.
        grok_home: crate::session::persistence::grok_home_string(),
        last_active_at: source.last_active_at,
        generated_title: source.generated_title,
        // A fork keeps the parent's title, so its manual-ness rides along.
        title_is_manual: source.title_is_manual,
        worktree_label: source.worktree_label,
        agent_name: source.agent_name,
        sandbox_profile: source.sandbox_profile,
        reasoning_effort: source.reasoning_effort,
        // Full forks keep the parent's last turn. Partial forks
        // (`target_prompt_index`) may drop that turn, so clear the summary
        // rather than showing work that is not in the child conversation.
        last_turn_summary: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_turn_summary
        },
        last_turn_summary_prompt_id: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_turn_summary_prompt_id
        },
    }
}

/// Copy one optional sidecar file (plan, signals, tool state, ...) when
/// enabled and present; reports whether a copy happened. A sidecar that
/// exists but is not a regular file is skipped with a warning rather than
/// failing the fork.
fn copy_sidecar_file(enabled: bool, src: &Path, dst: &Path) -> io::Result<bool> {
    if !enabled {
        return Ok(false);
    }
    if !src.is_file() {
        if src.exists() {
            tracing::warn!(
                path = %src.display(),
                "sidecar is not a regular file; skipping copy",
            );
        }
        return Ok(false);
    }
    std::fs::copy(src, dst)?;
    Ok(true)
}

/// Copy the `compaction_checkpoints/{uuid}.json` files referenced by the
/// retained records; returns how many copied. Records are user-editable data,
/// so only the exact path shape this feature writes may resolve, symlinks are
/// never followed, and dangling references are skipped rather than failing
/// the fork (otherwise every /rewind in the target session would fail).
fn copy_referenced_checkpoints(
    checkpoint_files: &BTreeSet<String>,
    source_session_dir: &Path,
    target_dir: &Path,
    source_id: &acp::SessionId,
    assets: &mut ForkImageAssets,
    validate_evidence: impl Fn(&[ConversationItem]) -> io::Result<()>,
) -> io::Result<usize> {
    if checkpoint_files.is_empty() {
        return Ok(0);
    }
    // The per-file `symlink_metadata` below only vets the final path
    // component, so the intermediate `compaction_checkpoints` dir must itself
    // be a real directory; a symlinked dir would resolve every matching name
    // outside the session.
    match std::fs::symlink_metadata(source_session_dir.join("compaction_checkpoints")) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(meta) => {
            tracing::warn!(
                file_type = ?meta.file_type(),
                session_id = %source_id,
                "compaction_checkpoints is not a real directory; skipping checkpoint copy",
            );
            return Ok(0);
        }
        // Dir gone means every record is dangling; same policy as a missing
        // checkpoint file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(
                session_id = %source_id,
                "compaction_checkpoints directory missing; skipping checkpoint copy",
            );
            return Ok(0);
        }
        Err(error) => return Err(error),
    }
    let mut copied = 0usize;
    for checkpoint_file in checkpoint_files {
        let relative = Path::new(checkpoint_file);
        // A doctored record path must not address other session files (e.g.
        // the fork's rewritten updates.jsonl).
        let well_formed = relative.parent() == Some(Path::new("compaction_checkpoints"))
            && relative.extension() == Some("json".as_ref());
        if !well_formed {
            tracing::warn!(
                checkpoint_file = %checkpoint_file,
                session_id = %source_id,
                "skipping compaction checkpoint with unexpected path during copy",
            );
            continue;
        }
        let src = source_session_dir.join(relative);
        match std::fs::symlink_metadata(&src) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(meta) => {
                // This feature only ever writes regular files, so don't
                // follow symlinks planted in the source session.
                tracing::warn!(
                    path = %src.display(),
                    file_type = ?meta.file_type(),
                    session_id = %source_id,
                    "compaction checkpoint source is not a regular file; skipping copy",
                );
                continue;
            }
            // Already-dangling record (e.g. a chained fork of a broken
            // session): the copy can't invent the file, so don't fail.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %src.display(),
                    session_id = %source_id,
                    "compaction checkpoint file missing from source; skipping copy",
                );
                continue;
            }
            Err(error) => return Err(error),
        }
        let dst = target_dir.join(relative);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = std::fs::read(&src)?;
        let mut checkpoint: crate::extensions::notification::CompactionCheckpointFile =
            serde_json::from_slice(&bytes).map_err(invalid_data)?;
        let rewritten = if checkpoint.schema_version == 1 {
            let mut changed = assets.rewrite_conversation(&mut checkpoint.compacted_history)?;
            for path in &mut checkpoint.reread_file_paths {
                if let Some(replacement) = assets.copy_asset(path)? {
                    *path = replacement;
                    changed = true;
                }
            }
            if changed {
                // Preserve additional schema-1 fields without introducing a migration.
                let mut value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(invalid_data)?;
                value["compacted_history"] =
                    serde_json::to_value(&checkpoint.compacted_history).map_err(invalid_data)?;
                value["reread_file_paths"] =
                    serde_json::to_value(&checkpoint.reread_file_paths).map_err(invalid_data)?;
                Some(serde_json::to_vec(&value).map_err(invalid_data)?)
            } else {
                None
            }
        } else {
            None
        };
        // Older checkpoints can retain assistants absent from the active chat.
        // Validate their final history against the child's evidence before writing.
        validate_evidence(&checkpoint.compacted_history)?;
        std::fs::write(&dst, rewritten.as_deref().unwrap_or(&bytes))?;
        copied += 1;
    }
    Ok(copied)
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
