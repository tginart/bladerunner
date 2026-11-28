//! On-disk hubs for `agent_hub`: members, channels, DMs and read positions.
//!
//! Layout under `<grok home>/hubs/<name>/`:
//! - `hub.json`: the hub name, members, channels and the loop-guard streak.
//! - `channels/<channel>.jsonl` and `dms/<a>__<b>.jsonl`: append-only messages.
//! - `cursors/<handle>.json`: per-member read and notify positions.
//! - `tokens/<handle>.json`: estimated context tokens that each member read.
//! - `presence/<handle>.json`: what each agent does now (working, idle, ...).
//! - `blobs/`: images attached to messages.
//! - `hub.lock`: an advisory lock that every write of `hub.json` or of a
//!   conversation holds. Cursor and token files use per-member locks.
//!
//! Every member process reads and appends to the same directory. No daemon is
//! needed: a writer holds `hub.lock`, and a reader reads up to the last full line.
//! A reader that saved a byte offset reads only the bytes after it.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Directory under the grok home that holds every hub.
pub const HUBS_DIR: &str = "hubs";
/// Channel that every hub starts with.
pub const GENERAL_CHANNEL: &str = "general";
/// Mention that wakes every active member of a channel (people only).
pub const HERE_MENTION: &str = "here";
/// Handle of hub-bot, the read-only helper that answers the person. It is not
/// a member in `hub.json`, so older binaries can still read the hub.
pub const BOT_HANDLE: &str = "hub-bot";
const BOT_DIR: &str = "bot";
/// Prefix of the hidden hub for one root session and its subagents.
const SESSION_HUB_PREFIX: &str = ".session-";
const META_FILE: &str = "hub.json";
const LOCK_FILE: &str = "hub.lock";
const MAX_NAME_CHARS: usize = 40;
const MAX_HANDLE_CHARS: usize = 64;
/// Most images one message can carry.
pub const MAX_IMAGES: usize = 4;
/// Largest image file one message can carry.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
/// Token-count key for all DMs of a hub.
pub const DM_TOKENS_KEY: &str = "dm";
const PRESENCE_DIR: &str = "presence";
const TOKENS_DIR: &str = "tokens";
/// Longest task text that a presence file keeps, in characters.
pub const MAX_TASK_CHARS: usize = 120;
/// Age after which an unused session hub of an ended session is removed.
const IDLE_SESSION_HUB_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Agent,
    Person,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub handle: String,
    pub label: String,
    pub kind: MemberKind,
    /// Session that owns this handle (root session or subagent id).
    #[serde(default)]
    pub session_id: Option<String>,
    pub active: bool,
    /// The root actor ended, but its membership remains available for resume.
    #[serde(default)]
    pub disconnected: bool,
    pub joined_ms: u64,
    /// Process that runs the session. A member whose process ended is gone.
    #[serde(default)]
    pub pid: Option<u32>,
}

impl Member {
    /// True when the member is active, connected, and its process still runs.
    pub fn is_live(&self) -> bool {
        self.active && !self.disconnected && self.pid.is_none_or(is_process_alive)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubMeta {
    pub name: String,
    pub created_ms: u64,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub members: Vec<Member>,
    /// Agent-to-agent wakes since a person last posted (loop guard).
    #[serde(default)]
    pub agent_wake_streak: u32,
    /// Estimated tokens that hub messages put into agents' context, keyed by
    /// `#channel` or [`DM_TOKENS_KEY`].
    #[serde(default)]
    pub tokens: BTreeMap<String, u64>,
    /// When true, a person's channel post wakes every member, as `@here` does.
    /// Hubs saved before this field existed load with the default, true.
    #[serde(default = "default_true")]
    pub person_posts_alert_all: bool,
}

fn default_true() -> bool {
    true
}

impl HubMeta {
    pub fn member(&self, handle: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.handle == handle)
    }

    pub fn has_channel(&self, name: &str) -> bool {
        self.channels.iter().any(|c| c.name == name)
    }

    pub fn is_session_hub(&self) -> bool {
        self.name.starts_with(SESSION_HUB_PREFIX)
    }
}

/// One message. `conv` is `#channel` or a DM key from [`dm_conv`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conv: String,
    pub from: String,
    pub text: String,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Handles named with `@` in the text, plus `here`.
    #[serde(default)]
    pub mentions: Vec<String>,
    /// Attached images: file names under the hub's `blobs/` directory.
    #[serde(default)]
    pub images: Vec<String>,
    pub ts_unix_ms: u64,
}

/// Per-member positions, counted in messages (lines) per conversation.
/// Note: A byte offset is a fast path. Its line count must match the cursor,
///     and its file stamp must retain the same file prefix. Older cursor files
///     without a stamp, or count-only changes, require a scan from line 0.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursors {
    /// Messages the member has read with `read` or `inbox`.
    #[serde(default)]
    pub read: BTreeMap<String, usize>,
    /// Messages already checked for DMs and mentions to deliver.
    #[serde(default)]
    pub notified: BTreeMap<String, usize>,
    /// Byte offset in `<conv>.jsonl` after the `read[conv]` complete lines.
    #[serde(default)]
    pub read_bytes: BTreeMap<String, LineOffset>,
    /// Byte offset in `<conv>.jsonl` after the `notified[conv]` complete lines.
    #[serde(default)]
    pub notified_bytes: BTreeMap<String, LineOffset>,
}

/// The byte offset in a conversation file after its first `lines` lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineOffset {
    pub lines: usize,
    pub bytes: u64,
    /// The file stamp that validates this offset. Older cursors have no stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<FileStamp>,
}

impl Cursors {
    /// The byte offset after the read messages of `conv`, when it is known.
    pub fn read_offset(&self, conv: &str) -> Option<LineOffset> {
        known_offset(&self.read, &self.read_bytes, conv)
    }

    /// The byte offset after the notified messages of `conv`, when it is known.
    pub fn notified_offset(&self, conv: &str) -> Option<LineOffset> {
        known_offset(&self.notified, &self.notified_bytes, conv)
    }

    /// Set the read count of `conv`, with its byte offset when it is known.
    pub fn set_read(&mut self, conv: &str, count: usize, offset: Option<LineOffset>) {
        set_position(&mut self.read, &mut self.read_bytes, conv, count, offset);
    }

    /// Set the notified count of `conv`, with its byte offset when it is known.
    pub fn set_notified(&mut self, conv: &str, count: usize, offset: Option<LineOffset>) {
        set_position(
            &mut self.notified,
            &mut self.notified_bytes,
            conv,
            count,
            offset,
        );
    }
}

/// Estimated context tokens that hub messages put into one member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberTokens {
    /// Tokens keyed by `#channel` or [`DM_TOKENS_KEY`].
    #[serde(default)]
    pub by_conv: BTreeMap<String, u64>,
}

/// The size, change time and identity of a file, from one `stat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// Device and inode on Unix. Other platforms use the creation time.
    pub identity: Option<(u64, u64)>,
}

impl FileStamp {
    /// Purpose: Check whether an append-only writer retains the saved prefix.
    /// Input: The current and previous file stamps.
    /// Output: True for an unchanged file or growth of the same file.
    fn retains_prefix(self, previous: Self) -> bool {
        self == previous
            || (self.identity == previous.identity
                && self.created == previous.created
                && self.len > previous.len)
    }
}

/// Messages of one conversation, read from a line index or a byte offset.
#[derive(Debug, Clone, Default)]
pub struct MessagesSlice {
    pub messages: Vec<Message>,
    /// Byte offset after the last returned message. `None` when the file
    /// has fewer complete lines than the start line.
    pub next_byte_offset: Option<LineOffset>,
    /// Complete messages after the limit that were not returned.
    pub remaining: usize,
    /// The file stamp from before the read.
    pub stamp: FileStamp,
}

impl MessagesSlice {
    /// The tail after the returned messages, for a read from `start_line`.
    pub fn tail(&self, start_line: usize) -> Option<ConvTail> {
        Some(ConvTail {
            byte_offset: self.next_byte_offset?.bytes,
            message_count: start_line + self.messages.len(),
            stamp: self.stamp,
        })
    }
}

/// Where an incremental reader of one conversation file stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvTail {
    /// Byte offset after the last complete message that was read.
    pub byte_offset: u64,
    /// Messages before `byte_offset`.
    pub message_count: usize,
    /// The file stamp of the last read. An equal stamp means no new bytes.
    pub stamp: FileStamp,
}

impl ConvTail {
    /// The file stamp and byte offset after the messages in this tail.
    pub fn offset(&self) -> LineOffset {
        LineOffset {
            lines: self.message_count,
            bytes: self.byte_offset,
            stamp: Some(self.stamp),
        }
    }
}

/// The result of [`Hub::read_tail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailRead {
    /// The file did not change.
    Unchanged,
    /// The messages after the old tail.
    Appended(Vec<Message>),
    /// The file became shorter or was replaced: every message, from line 0.
    Reset(Vec<Message>),
}

/// What a live agent does now, as its session reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// A turn runs.
    Working,
    /// No turn runs.
    Idle,
    /// The agent waits in `agent_hub wait` for a message.
    Waiting,
    /// A permission prompt, a question, or a plan approval waits for a person.
    NeedsInput,
}

impl Activity {
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Working => "working",
            Activity::Idle => "idle",
            Activity::Waiting => "waiting",
            Activity::NeedsInput => "needs_input",
        }
    }
}

/// The contents of one presence file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub activity: Activity,
    /// When the activity started.
    pub since_ms: u64,
    /// The prompt of the last turn that a person or a parent agent started.
    #[serde(default)]
    pub task: Option<String>,
}

/// A handle to one hub directory.
/// The conversation of hub-bot with one person: the last child run, which the
/// next question resumes when it comes from the same session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotThread {
    pub session_id: String,
    pub child_id: String,
}

#[derive(Debug, Clone)]
pub struct Hub {
    dir: PathBuf,
    name: String,
}

impl Hub {
    /// Create a new hub with `#general`. Fails when the hub exists.
    pub fn create(hubs_root: &Path, name: &str) -> Result<Hub> {
        let hub = Self::at(hubs_root, name)?;
        if hub.dir.join(META_FILE).exists() {
            bail!("hub {name:?} already exists");
        }
        hub.init()?;
        Ok(hub)
    }

    /// Open an existing hub. Fails when the hub does not exist.
    pub fn open(hubs_root: &Path, name: &str) -> Result<Hub> {
        let hub = Self::at(hubs_root, name)?;
        if !hub.dir.join(META_FILE).exists() {
            bail!("hub {name:?} does not exist");
        }
        Ok(hub)
    }

    /// Purpose: Open an existing named hub or session hub by its directory name.
    /// Input: The hubs root and the name that `Hub::name` gives.
    /// Output: The hub. Fails for a name that is not a hub name.
    pub fn open_by_name(hubs_root: &Path, name: &str) -> Result<Hub> {
        let Some(session_id) = name.strip_prefix(SESSION_HUB_PREFIX) else {
            return Self::open(hubs_root, name);
        };
        let is_valid = !session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !is_valid {
            bail!("invalid hub name {name:?}");
        }
        Self::open_existing_session_hub(hubs_root, session_id)
    }

    /// Open the hidden hub of a root session, and create it on first use.
    pub fn open_session_hub(hubs_root: &Path, root_session_id: &str) -> Result<Hub> {
        let name = session_hub_name(root_session_id);
        let hub = Hub {
            dir: hubs_root.join(&name),
            name,
        };
        if !hub.dir.join(META_FILE).exists() {
            hub.init()?;
        }
        Ok(hub)
    }

    /// Open the hidden hub of a root session. Fail when it does not exist.
    pub fn open_existing_session_hub(hubs_root: &Path, root_session_id: &str) -> Result<Hub> {
        let name = session_hub_name(root_session_id);
        let hub = Hub {
            dir: hubs_root.join(&name),
            name,
        };
        if !hub.dir.join(META_FILE).exists() {
            bail!("this session has no hub");
        }
        Ok(hub)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// True when some channel or DM of the hub holds a message.
    pub fn has_messages(&self) -> bool {
        has_messages(&self.dir.join("channels")) || has_messages(&self.dir.join("dms"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn meta(&self) -> Result<HubMeta> {
        read_json(&self.dir.join(META_FILE))
    }

    /// Change `hub.json` under the hub lock and return the closure's result.
    pub fn update_meta<T>(&self, change: impl FnOnce(&mut HubMeta) -> T) -> Result<T> {
        let _lock = self.lock()?;
        let mut meta = self.meta()?;
        let out = change(&mut meta);
        write_json_atomic(&self.dir.join(META_FILE), &meta)?;
        Ok(out)
    }

    /// Append one message under the hub lock.
    pub fn append(&self, message: &Message) -> Result<()> {
        let path = self.conv_path(&message.conv)?;
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        let _lock = self.lock()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Messages of `conv` from index `start` on. A partial last line is skipped.
    pub fn messages(&self, conv: &str, start: usize) -> Result<Vec<Message>> {
        Ok(self.messages_from_offset(conv, start, None, None)?.messages)
    }

    /// Purpose: Read the messages of `conv` from line `start_line` on.
    /// Input: The conversation, the start line, the byte offset of that line
    ///     when it is known, and an optional maximum count.
    /// Output: The messages, the byte offset after the last of them, and the
    ///     count of complete messages after the limit.
    /// Method:
    ///     1. If the saved stamp retains its prefix and the offset starts a line,
    ///        seek to it.
    ///     2. If not (a legacy cursor, or a shorter or replaced file), scan
    ///        from byte 0 past `start_line` lines.
    ///     3. Read complete lines up to `limit`. Stop at a line without a
    ///        newline or with invalid JSON: that is a write in progress.
    pub fn messages_from_offset(
        &self,
        conv: &str,
        start_line: usize,
        start_byte: Option<LineOffset>,
        limit: Option<usize>,
    ) -> Result<MessagesSlice> {
        let Some((mut file, stamp)) = self.open_conv(conv)? else {
            return Ok(MessagesSlice {
                next_byte_offset: (start_line == 0).then_some(LineOffset::default()),
                ..MessagesSlice::default()
            });
        };
        let seek_to = match start_byte {
            Some(offset)
                if offset.lines == start_line
                    && offset
                        .stamp
                        .is_some_and(|previous| stamp.retains_prefix(previous))
                    && is_line_start(&mut file, offset.bytes, stamp.len)? =>
            {
                Some(offset.bytes)
            }
            _ => None,
        };
        let mut reader = BufReader::new(file);
        let start = match seek_to {
            Some(offset) => offset,
            None => {
                reader.seek(SeekFrom::Start(0))?;
                let Some(offset) = skip_lines(&mut reader, start_line)? else {
                    return Ok(MessagesSlice {
                        stamp,
                        ..MessagesSlice::default()
                    });
                };
                offset
            }
        };
        reader.seek(SeekFrom::Start(start))?;
        let (messages, end, remaining) = read_messages(&mut reader, start, limit)?;
        let next_byte_offset = Some(LineOffset {
            lines: start_line + messages.len(),
            bytes: end,
            stamp: Some(stamp),
        });
        Ok(MessagesSlice {
            messages,
            next_byte_offset,
            remaining,
            stamp,
        })
    }

    /// Purpose: Read the messages that were appended to `conv` after `tail`.
    /// Input: The conversation and the tail of the last read.
    /// Output: Nothing new when the file stamp did not change, the appended
    ///     messages, or every message when the file became shorter or was
    ///     replaced. The tail moves past the messages that were read.
    pub fn read_tail(&self, conv: &str, tail: &mut ConvTail) -> Result<TailRead> {
        let Some((mut file, stamp)) = self.open_conv(conv)? else {
            let was_empty = tail.message_count == 0;
            *tail = ConvTail::default();
            return Ok(if was_empty {
                TailRead::Unchanged
            } else {
                TailRead::Reset(Vec::new())
            });
        };
        if stamp == tail.stamp {
            return Ok(TailRead::Unchanged);
        }
        // Supported writers only append. A new inode or a changed file that
        // did not grow cannot retain the previous file's cached messages.
        let is_append = stamp.retains_prefix(tail.stamp)
            && is_line_start(&mut file, tail.byte_offset, stamp.len)?;
        let start = if is_append { tail.byte_offset } else { 0 };
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(start))?;
        let (messages, end, _) = read_messages(&mut reader, start, None)?;
        let base = if is_append { tail.message_count } else { 0 };
        *tail = ConvTail {
            byte_offset: end,
            message_count: base + messages.len(),
            stamp,
        };
        Ok(if is_append {
            TailRead::Appended(messages)
        } else {
            TailRead::Reset(messages)
        })
    }

    /// Purpose: Stat the files that a new message or a member change touches.
    /// Input: The hub.
    /// Output: The stamp of `hub.json` and of every conversation file, sorted
    ///     by path. A file that cannot be read is left out.
    /// Note: This opens no file and takes no lock.
    pub fn file_stamps(&self) -> Vec<(PathBuf, FileStamp)> {
        let mut paths = vec![self.dir.join(META_FILE)];
        for sub in ["channels", "dms"] {
            if let Ok(entries) = fs::read_dir(self.dir.join(sub)) {
                paths.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
        let mut out = Vec::new();
        for path in paths {
            if let Ok(metadata) = fs::metadata(&path) {
                out.push((path, file_stamp(&metadata)));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Purpose: Add token costs to the token file of one member.
    /// Input: The member handle and the costs by token key.
    /// Output: Ok when the file is saved.
    /// Note: The caller holds the member's cursor lock. No hub lock is taken
    ///     and `hub.json` does not change.
    pub fn add_member_tokens(&self, handle: &str, costs: &BTreeMap<String, u64>) -> Result<()> {
        if costs.is_empty() {
            return Ok(());
        }
        create_private_dir(&self.dir.join(TOKENS_DIR))?;
        let path = self.tokens_path(handle);
        let mut tokens: MemberTokens = if path.exists() {
            read_json(&path)?
        } else {
            MemberTokens::default()
        };
        for (key, cost) in costs {
            *tokens.by_conv.entry(key.clone()).or_default() += cost;
        }
        write_json_atomic(&path, &tokens)
    }

    /// Purpose: Sum the token counts of the hub.
    /// Input: The hub metadata, which holds the counts from before token files.
    /// Output: The counts by token key: `meta.tokens` plus every member file.
    pub fn tokens(&self, meta: &HubMeta) -> Result<BTreeMap<String, u64>> {
        let mut out = meta.tokens.clone();
        let entries = match fs::read_dir(self.dir.join(TOKENS_DIR)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let path = entry?.path();
            // A temp file of a write in progress has another extension.
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let tokens: MemberTokens = read_json(&path)?;
            for (key, cost) in tokens.by_conv {
                *out.entry(key).or_default() += cost;
            }
        }
        Ok(out)
    }

    pub fn cursors(&self, handle: &str) -> Result<Cursors> {
        let path = self.cursor_path(handle);
        if !path.exists() {
            return Ok(Cursors::default());
        }
        read_json(&path)
    }

    /// Hold this lock across the complete cursor read, change, and save transaction.
    pub(super) fn lock_cursors(&self, handle: &str) -> Result<File> {
        let path = self.cursor_path(handle).with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(file)
    }

    /// Save cursors while the caller holds the member's cursor lock.
    pub fn save_cursors(&self, handle: &str, cursors: &Cursors) -> Result<()> {
        write_json_atomic(&self.cursor_path(handle), cursors)
    }

    /// Purpose: Copy an image file into the hub so every member can read it.
    /// Input: The path of a local png, jpg, gif or webp file.
    /// Output: The new file name under `blobs/`.
    pub fn store_image(&self, source: &Path) -> Result<String> {
        let extension = source
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
            bail!(
                "{} is not an image (use {})",
                source.display(),
                IMAGE_EXTENSIONS.join(", ")
            );
        }
        let size = fs::metadata(source)
            .with_context(|| format!("read {}", source.display()))?
            .len();
        if size > MAX_IMAGE_BYTES {
            bail!("{} is larger than 5 MiB", source.display());
        }
        let blobs = self.dir.join("blobs");
        create_private_dir(&blobs)?;
        let name = format!(
            "{}.{extension}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        );
        fs::copy(source, blobs.join(&name))
            .with_context(|| format!("copy {}", source.display()))?;
        Ok(name)
    }

    pub fn blob_path(&self, name: &str) -> PathBuf {
        self.dir.join("blobs").join(name)
    }

    /// DM conversations that include `handle`.
    pub fn dm_convs_for(&self, handle: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(self.dir.join("dms")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        let mine = encode_handle(handle);
        for entry in entries {
            let file_name = entry?.file_name().to_string_lossy().into_owned();
            let Some(stem) = file_name.strip_suffix(".jsonl") else {
                continue;
            };
            let Some((a, b)) = stem.split_once("__") else {
                continue;
            };
            if a == mine || b == mine {
                out.push(dm_conv(&decode_handle(a), &decode_handle(b)));
            }
        }
        out.sort();
        Ok(out)
    }

    fn at(hubs_root: &Path, name: &str) -> Result<Hub> {
        if !is_valid_hub_name(name) {
            bail!(
                "invalid hub name {name:?}: use 1-{MAX_NAME_CHARS} lowercase letters, digits, - or _"
            );
        }
        Ok(Hub {
            dir: hubs_root.join(name),
            name: name.to_string(),
        })
    }

    fn init(&self) -> Result<()> {
        for sub in ["channels", "dms", "cursors"] {
            create_private_dir(&self.dir.join(sub))?;
        }
        let now = now_ms();
        let meta = HubMeta {
            name: self.name.clone(),
            created_ms: now,
            archived: false,
            channels: vec![Channel {
                name: GENERAL_CHANNEL.to_string(),
                purpose: "Status and coordination for everyone in the hub".to_string(),
                created_ms: now,
            }],
            members: Vec::new(),
            agent_wake_streak: 0,
            tokens: BTreeMap::new(),
            person_posts_alert_all: true,
        };
        let _lock = self.lock()?;
        if !self.dir.join(META_FILE).exists() {
            write_json_atomic(&self.dir.join(META_FILE), &meta)?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<File> {
        let path = self.dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(file)
    }

    /// Open a conversation file and stat it, or `None` when it does not exist.
    fn open_conv(&self, conv: &str) -> Result<Option<(File, FileStamp)>> {
        let path = self.conv_path(conv)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let stamp = file_stamp(&file.metadata()?);
        Ok(Some((file, stamp)))
    }

    fn conv_path(&self, conv: &str) -> Result<PathBuf> {
        if let Some(channel) = conv.strip_prefix('#') {
            return Ok(self.dir.join("channels").join(format!("{channel}.jsonl")));
        }
        let Some((a, b)) = split_dm_conv(conv) else {
            bail!("invalid conversation {conv:?}");
        };
        let file = format!("{}__{}.jsonl", encode_handle(a), encode_handle(b));
        Ok(self.dir.join("dms").join(file))
    }

    /// The presence of a member, or None when it has not reported one.
    /// A presence file that cannot be read counts as none.
    pub fn presence(&self, handle: &str) -> Option<Presence> {
        read_json(&self.presence_path(handle)).ok()
    }

    /// Purpose: Read the presence of every member that reported one.
    /// Input: The hub.
    /// Output: Presence by handle. Files that cannot be read are left out.
    pub fn presences(&self) -> BTreeMap<String, Presence> {
        let mut out = BTreeMap::new();
        let Ok(entries) = fs::read_dir(self.dir.join(PRESENCE_DIR)) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if let Ok(presence) = read_json(&path) {
                out.insert(decode_handle(stem), presence);
            }
        }
        out
    }

    /// Save the presence of a member. The file is replaced atomically.
    pub fn save_presence(&self, handle: &str, presence: &Presence) -> Result<()> {
        create_private_dir(&self.dir.join(PRESENCE_DIR))?;
        write_json_atomic(&self.presence_path(handle), presence)
    }

    /// The last hub-bot run for a person, or None before the first question.
    pub fn bot_thread(&self, person: &str) -> Option<BotThread> {
        read_json(&self.bot_thread_path(person)).ok()
    }

    /// Save the last hub-bot run for a person. The file is replaced atomically.
    pub fn save_bot_thread(&self, person: &str, thread: &BotThread) -> Result<()> {
        create_private_dir(&self.dir.join(BOT_DIR))?;
        write_json_atomic(&self.bot_thread_path(person), thread)
    }

    fn bot_thread_path(&self, person: &str) -> PathBuf {
        self.dir
            .join(BOT_DIR)
            .join(format!("{}.json", encode_handle(person)))
    }

    fn presence_path(&self, handle: &str) -> PathBuf {
        self.dir
            .join(PRESENCE_DIR)
            .join(format!("{}.json", encode_handle(handle)))
    }

    fn cursor_path(&self, handle: &str) -> PathBuf {
        self.dir
            .join("cursors")
            .join(format!("{}.json", encode_handle(handle)))
    }

    fn tokens_path(&self, handle: &str) -> PathBuf {
        self.dir
            .join(TOKENS_DIR)
            .join(format!("{}.json", encode_handle(handle)))
    }
}

/// Purpose: Find the messages of a hub that contain a query.
/// Input: The hub, the query, and the maximum count.
/// Output: Up to `limit` messages, newest first, whose text or sender
///     contains the query, ignoring case. Every channel and DM is searched.
///     No cursor moves.
pub fn search(hub: &Hub, query: &str, limit: usize) -> Result<Vec<Message>> {
    search_where(hub, query, limit, |_| true)
}

/// Purpose: Find the messages of a hub that contain a query and pass a filter.
/// Input: The hub, the query, the maximum count, and the filter.
/// Output: As `search`. A message that the filter refuses does not count
///     toward `limit`.
pub fn search_where(
    hub: &Hub,
    query: &str,
    limit: usize,
    keep: impl Fn(&Message) -> bool,
) -> Result<Vec<Message>> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for message in recent_messages(hub, None, usize::MAX)?.into_iter().rev() {
        let is_hit = message.text.to_lowercase().contains(&query)
            || format!("@{}", message.from).to_lowercase().contains(&query);
        if is_hit && keep(&message) {
            hits.push(message);
            if hits.len() == limit {
                break;
            }
        }
    }
    Ok(hits)
}

/// Purpose: Read the latest messages of a hub for a person to look at.
/// Input: The hub, an optional channel (all channels and DMs when `None`),
///     and the maximum count.
/// Output: Up to `limit` messages, oldest first. No cursor moves.
pub fn recent_messages(hub: &Hub, channel: Option<&str>, limit: usize) -> Result<Vec<Message>> {
    let meta = hub.meta()?;
    let mut convs: Vec<String> = Vec::new();
    match channel {
        Some(raw) => {
            let Some(name) = normalize_channel(raw) else {
                bail!("invalid channel {raw:?}");
            };
            if !meta.has_channel(&name) {
                bail!("unknown channel #{name}");
            }
            convs.push(format!("#{name}"));
        }
        None => {
            convs.extend(meta.channels.iter().map(|c| format!("#{}", c.name)));
            for member in &meta.members {
                for conv in hub.dm_convs_for(&member.handle)? {
                    if !convs.contains(&conv) {
                        convs.push(conv);
                    }
                }
            }
        }
    }
    let mut messages = Vec::new();
    for conv in &convs {
        messages.extend(hub.messages(conv, 0)?);
    }
    messages.sort_by_key(|m| m.ts_unix_ms);
    let skip = messages.len().saturating_sub(limit);
    Ok(messages.split_off(skip))
}

/// The handle a person posts under: the OS user name, or `person`.
pub fn person_handle() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    normalize_handle(&user)
        .filter(|h| !h.contains('/'))
        .unwrap_or_else(|| "person".to_string())
}

/// The token-count key of a conversation: `#channel`, or [`DM_TOKENS_KEY`].
pub fn tokens_key(conv: &str) -> String {
    if split_dm_conv(conv).is_some() {
        DM_TOKENS_KEY.to_string()
    } else {
        conv.to_string()
    }
}

/// The DM conversation key for two handles, the same in either order.
pub fn dm_conv(a: &str, b: &str) -> String {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    format!("@{first},@{second}")
}

/// The two handles of a DM key, or `None` for a channel key.
pub fn split_dm_conv(conv: &str) -> Option<(&str, &str)> {
    let (a, b) = conv.split_once(',')?;
    Some((a.strip_prefix('@')?, b.strip_prefix('@')?))
}

pub fn session_hub_name(root_session_id: &str) -> String {
    format!("{SESSION_HUB_PREFIX}{root_session_id}")
}

/// Visible hubs on disk, sorted by name. Session hubs are never listed.
pub fn list_hubs(hubs_root: &Path, include_archived: bool) -> Result<Vec<HubMeta>> {
    let mut out = Vec::new();
    for name in hub_names(hubs_root)? {
        let Ok(hub) = Hub::open(hubs_root, &name) else {
            continue;
        };
        let meta = hub.meta()?;
        if include_archived || !meta.archived {
            out.push(meta);
        }
    }
    Ok(out)
}

/// Purpose: Remove the session hubs that no session uses.
/// Input: The hubs root and the session id of the caller, whose hub stays.
/// Output: The number of session hubs that this function removed.
/// Method:
///     1. Find each hidden session hub that is older than IDLE_SESSION_HUB_MS.
///     2. Under the hub lock, check its age, live members, and messages.
///     3. Remove its directory only if the hub is still idle.
/// Note: Every session makes a session hub, so without this they pile up.
///     A resumed session makes its session hub again when it needs it.
pub fn prune_idle_session_hubs(hubs_root: &Path, keep_session_id: &str) -> Result<usize> {
    let entries = match fs::read_dir(hubs_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let keep = session_hub_name(keep_session_id);
    let mut removed = 0;
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !name.starts_with(SESSION_HUB_PREFIX) || name == keep {
            continue;
        }
        let hub = Hub {
            dir: hubs_root.join(&name),
            name,
        };
        let _lock = hub.lock()?;
        if is_idle_session_hub(&hub) {
            fs::remove_dir_all(&hub.dir)
                .with_context(|| format!("remove {}", hub.dir.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Purpose: Find the visible hub where a session is an active member.
/// Input: The hubs root and a session id.
/// Output: The hub name and the member handle, or `None`.
/// Note: The shell uses this on resume and on fork. Archived hubs are skipped.
pub fn find_membership(hubs_root: &Path, session_id: &str) -> Option<(String, String)> {
    for meta in list_hubs(hubs_root, false).ok()? {
        let found = meta
            .members
            .iter()
            .find(|m| m.active && m.session_id.as_deref() == Some(session_id));
        if let Some(member) = found {
            return Some((meta.name.clone(), member.handle.clone()));
        }
    }
    None
}

/// The closest visible hub name to a name that does not exist.
pub fn suggest_hub_name(hubs_root: &Path, name: &str) -> Option<String> {
    let names = hub_names(hubs_root).ok()?;
    names
        .into_iter()
        .map(|candidate| (edit_distance(name, &candidate), candidate))
        .filter(|(distance, _)| *distance <= 3)
        .min()
        .map(|(_, candidate)| candidate)
}

/// A handle with any leading `@` removed, or `None` when it is not valid.
pub fn normalize_handle(raw: &str) -> Option<String> {
    let handle = raw.trim().trim_start_matches('@');
    let is_valid = !handle.is_empty()
        && handle.chars().count() <= MAX_HANDLE_CHARS
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    is_valid.then(|| handle.to_string())
}

/// A channel name with any leading `#` removed, or `None` when it is not valid.
pub fn normalize_channel(raw: &str) -> Option<String> {
    let name = raw.trim().trim_start_matches('#');
    is_valid_hub_name(name).then(|| name.to_string())
}

/// `base`, or `base-2`, `base-3`, ... when another live session holds the handle.
pub fn unique_handle(meta: &HubMeta, base: &str, session_id: Option<&str>) -> String {
    let is_free = |handle: &str| match meta.member(handle) {
        None => true,
        Some(member) => {
            !member.is_live()
                || (member.session_id.as_deref() == session_id && session_id.is_some())
        }
    };
    let base = handle_candidate(base, "");
    if is_free(&base) {
        return base;
    }
    (2..)
        .map(|n| handle_candidate(&base, &format!("-{n}")))
        .find(|candidate| is_free(candidate))
        .expect("an unused handle exists")
}

/// Purpose: Keep a generated handle within the recipient handle limit.
/// Input: The preferred handle and a uniqueness suffix.
/// Output: A handle of at most MAX_HANDLE_CHARS characters, with its namespace and suffix preserved.
pub fn handle_candidate(base: &str, suffix: &str) -> String {
    let budget = MAX_HANDLE_CHARS - suffix.chars().count();
    assert!(budget >= 3, "the suffix must leave room for a handle");
    let base = match base.rsplit_once('/') {
        Some((namespace, label)) => {
            let label_len = label.chars().count().min(budget - 2);
            let namespace_len = budget - label_len - 1;
            format!(
                "{}/{}",
                namespace.chars().take(namespace_len).collect::<String>(),
                label.chars().take(label_len).collect::<String>()
            )
        }
        None => base.chars().take(budget).collect(),
    };
    format!("{base}{suffix}")
}

/// Purpose: Find the members that a message names with `@`.
/// Input: The message text and the hub members.
/// Output: The named handles, plus `here` when the text has `@here`.
/// Note: A handle must be a whole word. `@backend` does not match `@backend/scout`.
/// Match the longest registered handle before removal of trailing sentence periods.
pub fn parse_mentions(text: &str, members: &[Member]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')'))
    {
        let Some(raw) = word.strip_prefix('@') else {
            continue;
        };
        let mut name = raw;
        while name != HERE_MENTION && !members.iter().any(|m| m.handle == name) {
            let Some(shorter) = name.strip_suffix(['.', '!', '?', '\'', '"']) else {
                break;
            };
            name = shorter;
        }
        let is_known = name == HERE_MENTION || members.iter().any(|m| m.handle == name);
        if is_known && !out.iter().any(|h| h == name) {
            out.push(name.to_string());
        }
    }
    out
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn is_valid_hub_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

fn hub_names(hubs_root: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(hubs_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// True when a session hub is old, has no live member, and holds no message.
fn is_idle_session_hub(hub: &Hub) -> bool {
    let Ok(meta) = hub.meta() else {
        return false;
    };
    let is_old = now_ms().saturating_sub(meta.created_ms) > IDLE_SESSION_HUB_MS;
    is_old && !meta.members.iter().any(Member::is_live) && !hub.has_messages()
}

fn known_offset(
    counts: &BTreeMap<String, usize>,
    offsets: &BTreeMap<String, LineOffset>,
    conv: &str,
) -> Option<LineOffset> {
    let count = counts.get(conv).copied().unwrap_or(0);
    if count == 0 {
        return Some(LineOffset::default());
    }
    offsets
        .get(conv)
        .filter(|offset| offset.lines == count && offset.stamp.is_some())
        .copied()
}

fn set_position(
    counts: &mut BTreeMap<String, usize>,
    offsets: &mut BTreeMap<String, LineOffset>,
    conv: &str,
    count: usize,
    offset: Option<LineOffset>,
) {
    counts.insert(conv.to_string(), count);
    match offset {
        Some(offset) => {
            assert_eq!(offset.lines, count);
            offsets.insert(conv.to_string(), offset);
        }
        None => {
            offsets.remove(conv);
        }
    }
}

fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        identity: Some((metadata.dev(), metadata.ino())),
        #[cfg(not(unix))]
        identity: None,
    }
}

/// True when `offset` is the start of a line: byte 0, or the byte after a newline.
fn is_line_start(file: &mut File, offset: u64, len: u64) -> Result<bool> {
    if offset == 0 {
        return Ok(true);
    }
    if offset > len {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset - 1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

/// The byte offset after `count` complete lines, or `None` when there are fewer.
fn skip_lines(reader: &mut impl BufRead, count: usize) -> Result<Option<u64>> {
    let mut offset = 0;
    let mut line = Vec::new();
    for _ in 0..count {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            return Ok(None);
        }
        offset += read as u64;
    }
    Ok(Some(offset))
}

/// Purpose: Read complete messages from a reader at byte `start`.
/// Input: The reader, its byte offset in the file, and an optional limit.
/// Output: Up to `limit` messages, the byte offset after the last of them,
///     and the count of complete messages after the limit.
/// Note: A line without a newline or a closing brace is a write in progress.
///     The read stops before it, so the next read gets the line whole.
fn read_messages(
    reader: &mut impl BufRead,
    start: u64,
    limit: Option<usize>,
) -> Result<(Vec<Message>, u64, usize)> {
    let mut messages = Vec::new();
    let mut end = start;
    let mut remaining = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        let Ok(message) = serde_json::from_slice::<Message>(&line) else {
            break;
        };
        if limit.is_some_and(|limit| messages.len() >= limit) {
            remaining += 1;
            continue;
        }
        end += read as u64;
        messages.push(message);
    }
    Ok((messages, end, remaining))
}

fn has_messages(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.metadata().is_ok_and(|m| m.len() > 0))
}

/// Use one-byte escapes outside the handle alphabet to keep DM filenames bounded.
fn encode_handle(handle: &str) -> String {
    handle.replace('/', "~").replace('_', "!")
}

fn decode_handle(encoded: &str) -> String {
    encoded.replace('!', "_").replace('~', "/")
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [path, path.parent().unwrap_or(path)] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// Purpose: Replace a JSON file so that no reader sees a partial file.
/// Input: The path and the value.
/// Output: Ok when the new file is in place.
/// Method:
///     1. Write the value to a temp file next to `path`. Its name holds the
///        process id and a sequence number, so no two writers share it, also
///        two threads of one process.
///     2. Rename the temp file to `path`.
///     3. On failure, remove the temp file.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .with_context(|| format!("no file name in {}", path.display()))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}-{sequence}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value)?;
    let result = fs::write(&tmp, bytes)
        .with_context(|| format!("write {}", tmp.display()))
        .and_then(|()| {
            fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
        });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b_chars.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != *cb);
            current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b_chars.len()]
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // EPERM means the process exists but belongs to another user.
    matches!(
        kill(Pid::from_raw(pid as i32), None),
        Ok(()) | Err(Errno::EPERM)
    )
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(conv: &str, from: &str, text: &str) -> Message {
        Message {
            id: format!("id-{text}"),
            conv: conv.to_string(),
            from: from.to_string(),
            text: text.to_string(),
            reply_to: None,
            mentions: Vec::new(),
            images: Vec::new(),
            ts_unix_ms: now_ms(),
        }
    }

    fn agent(handle: &str) -> Member {
        Member {
            handle: handle.to_string(),
            label: handle.to_string(),
            kind: MemberKind::Agent,
            session_id: Some(format!("sid-{handle}")),
            active: true,
            disconnected: false,
            joined_ms: 0,
            pid: None,
        }
    }

    #[test]
    fn create_open_and_list() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "api-rewrite").unwrap();
        assert!(hub.meta().unwrap().has_channel(GENERAL_CHANNEL));
        assert!(Hub::create(root.path(), "api-rewrite").is_err());
        assert!(Hub::open(root.path(), "missing").is_err());
        assert!(Hub::create(root.path(), "Bad Name").is_err());
        Hub::open_session_hub(root.path(), "abc").unwrap();

        let names: Vec<_> = list_hubs(root.path(), false)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["api-rewrite"]);
    }

    #[test]
    fn append_and_read_messages() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        hub.append(&message("#general", "a", "one")).unwrap();
        hub.append(&message("#general", "b", "two")).unwrap();
        let all = hub.messages("#general", 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(hub.messages("#general", 1).unwrap()[0].text, "two");
        assert!(hub.messages("#empty", 0).unwrap().is_empty());
    }

    #[test]
    fn dm_conversations_with_subagent_handles() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        let conv = dm_conv("backend/scout", "frontend");
        assert_eq!(conv, dm_conv("frontend", "backend/scout"));
        hub.append(&message(&conv, "frontend", "hi")).unwrap();
        assert_eq!(
            hub.dm_convs_for("backend/scout").unwrap(),
            vec![conv.clone()]
        );
        assert_eq!(hub.dm_convs_for("frontend").unwrap(), vec![conv.clone()]);
        assert!(hub.dm_convs_for("other").unwrap().is_empty());
        assert_eq!(hub.messages(&conv, 0).unwrap()[0].text, "hi");
    }

    #[test]
    fn dm_filenames_escape_underscores_without_aliases() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        for (a, b) in [("a__b", "z"), ("a", "b__z"), ("a_/scout", "z_")] {
            let conv = dm_conv(a, b);
            hub.append(&message(&conv, a, a)).unwrap();
            assert_eq!(hub.dm_convs_for(a).unwrap(), vec![conv.clone()]);
            assert_eq!(hub.dm_convs_for(b).unwrap(), vec![conv.clone()]);
            assert_eq!(hub.messages(&conv, 0).unwrap()[0].from, a);
        }
        assert_eq!(hub.dm_convs_for("a").unwrap(), vec![dm_conv("a", "b__z")]);
        assert_eq!(hub.dm_convs_for("z").unwrap(), vec![dm_conv("a__b", "z")]);
    }

    #[test]
    fn maximum_length_handles_fit_in_dm_filenames() {
        let root = tempfile::tempdir().unwrap();
        for (index, (a, b)) in [
            ("_".repeat(64), "a".repeat(64)),
            ("_".repeat(64), "b_".repeat(32)),
            ("_/".repeat(32), "c_".repeat(32)),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(normalize_handle(&a), Some(a.clone()));
            assert_eq!(normalize_handle(&b), Some(b.clone()));
            let hub = Hub::create(root.path(), &format!("h{index}")).unwrap();
            let conv = dm_conv(&a, &b);
            let path = hub.conv_path(&conv).unwrap();
            assert_eq!(path.file_name().unwrap().as_encoded_bytes().len(), 136);
            hub.append(&message(&conv, &a, "long handle DM")).unwrap();
            assert_eq!(hub.dm_convs_for(&a).unwrap(), vec![conv.clone()]);
            assert_eq!(hub.dm_convs_for(&b).unwrap(), vec![conv.clone()]);
            assert_eq!(hub.messages(&conv, 0).unwrap()[0].text, "long handle DM");
            let _cursor_lock = hub.lock_cursors(&a).unwrap();
            hub.save_cursors(&a, &Cursors::default()).unwrap();
        }
    }

    #[test]
    fn images_are_copied_into_blobs() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        let png = root.path().join("shot.PNG");
        std::fs::write(&png, b"fake png").unwrap();
        let name = hub.store_image(&png).unwrap();
        assert!(name.ends_with(".png"));
        assert_eq!(std::fs::read(hub.blob_path(&name)).unwrap(), b"fake png");
        let txt = root.path().join("notes.txt");
        std::fs::write(&txt, b"x").unwrap();
        assert!(hub.store_image(&txt).is_err());
        assert_eq!(tokens_key("#general"), "#general");
        assert_eq!(tokens_key(&dm_conv("a", "b")), DM_TOKENS_KEY);
    }

    #[test]
    fn cursors_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        assert_eq!(hub.cursors("a").unwrap(), Cursors::default());
        let mut cursors = Cursors::default();
        cursors.read.insert("#general".into(), 3);
        let _cursor_lock = hub.lock_cursors("a/b").unwrap();
        hub.save_cursors("a/b", &cursors).unwrap();
        assert_eq!(hub.cursors("a/b").unwrap(), cursors);
    }

    fn append_raw(hub: &Hub, conv: &str, bytes: &[u8]) {
        let path = hub.conv_path(conv).unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    fn line(conv: &str, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::to_string(&message(conv, "a", text)).unwrap()
        )
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.text.as_str()).collect()
    }

    #[test]
    fn byte_offset_reads_return_only_appended_messages() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        hub.append(&message("#general", "a", "one")).unwrap();
        hub.append(&message("#general", "a", "two")).unwrap();
        let first = hub.messages_from_offset("#general", 0, None, None).unwrap();
        assert_eq!(texts(&first.messages), vec!["one", "two"]);
        let offset = first.next_byte_offset.unwrap();
        assert_eq!(offset.bytes, first.stamp.len);

        hub.append(&message("#general", "a", "three")).unwrap();
        let next = hub
            .messages_from_offset("#general", 2, Some(offset), None)
            .unwrap();
        assert_eq!(texts(&next.messages), vec!["three"]);
        // A scan from line 0 finds the same messages and the same offset.
        let scanned = hub.messages_from_offset("#general", 2, None, None).unwrap();
        assert_eq!(scanned.messages, next.messages);
        assert_eq!(scanned.next_byte_offset, next.next_byte_offset);
        let tail = next.tail(2).unwrap();
        assert_eq!(tail.message_count, 3);

        let missing = hub.messages_from_offset("#none", 0, None, None).unwrap();
        assert_eq!(missing.next_byte_offset.map(|offset| offset.bytes), Some(0));
        let past_end = hub.messages_from_offset("#general", 9, None, None).unwrap();
        assert!(past_end.messages.is_empty());
        assert_eq!(past_end.next_byte_offset, None);
    }

    #[test]
    fn a_partial_trailing_line_waits_for_its_newline() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        let full = line("#general", "one");
        let partial = line("#general", "two");
        let (head, rest) = partial.split_at(10);
        append_raw(&hub, "#general", full.as_bytes());
        append_raw(&hub, "#general", head.as_bytes());

        let slice = hub.messages_from_offset("#general", 0, None, None).unwrap();
        assert_eq!(texts(&slice.messages), vec!["one"]);
        assert_eq!(
            slice.next_byte_offset.map(|offset| offset.bytes),
            Some(full.len() as u64)
        );
        let mut tail = slice.tail(0).unwrap();
        assert_eq!(
            hub.read_tail("#general", &mut tail).unwrap(),
            TailRead::Unchanged
        );

        append_raw(&hub, "#general", rest.as_bytes());
        let read = hub.read_tail("#general", &mut tail).unwrap();
        let TailRead::Appended(messages) = read else {
            panic!("expected an append, got {read:?}");
        };
        assert_eq!(texts(&messages), vec!["two"]);
        assert_eq!(tail.message_count, 2);
        assert_eq!(tail.byte_offset, (full.len() + partial.len()) as u64);
        assert_eq!(
            hub.read_tail("#general", &mut tail).unwrap(),
            TailRead::Unchanged
        );
    }

    #[test]
    fn a_limit_stops_the_offset_after_the_last_returned_message() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        for text in ["a", "b", "c"] {
            hub.append(&message("#general", "x", text)).unwrap();
        }
        let first = hub
            .messages_from_offset("#general", 0, None, Some(1))
            .unwrap();
        assert_eq!(texts(&first.messages), vec!["a"]);
        assert_eq!(first.remaining, 2);
        assert_eq!(
            first.next_byte_offset.map(|offset| offset.bytes),
            Some(line("#general", "a").len() as u64)
        );
        let rest = hub
            .messages_from_offset("#general", 1, first.next_byte_offset, None)
            .unwrap();
        assert_eq!(texts(&rest.messages), vec!["b", "c"]);
        assert_eq!(rest.remaining, 0);
    }

    #[test]
    fn a_shorter_or_replaced_file_reads_again_from_line_zero() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        for text in ["a", "b", "c"] {
            hub.append(&message("#general", "x", text)).unwrap();
        }
        let mut tail = ConvTail::default();
        hub.read_tail("#general", &mut tail).unwrap();
        let old_offset = tail.byte_offset;
        let path = hub.conv_path("#general").unwrap();
        fs::write(&path, line("#general", "new")).unwrap();

        // A saved offset past the end falls back to a scan by line count.
        let slice = hub
            .messages_from_offset(
                "#general",
                0,
                Some(LineOffset {
                    lines: 0,
                    bytes: old_offset,
                    stamp: Some(tail.stamp),
                }),
                None,
            )
            .unwrap();
        assert_eq!(texts(&slice.messages), vec!["new"]);
        let read = hub.read_tail("#general", &mut tail).unwrap();
        assert_eq!(read, TailRead::Reset(vec![slice.messages[0].clone()]));
        assert_eq!(tail.message_count, 1);

        // An offset that is not at a line start is not used either.
        let mid_line = hub
            .messages_from_offset(
                "#general",
                0,
                Some(LineOffset {
                    lines: 0,
                    bytes: 3,
                    stamp: Some(slice.stamp),
                }),
                None,
            )
            .unwrap();
        assert_eq!(texts(&mid_line.messages), vec!["new"]);

        fs::remove_file(&path).unwrap();
        assert_eq!(
            hub.read_tail("#general", &mut tail).unwrap(),
            TailRead::Reset(Vec::new())
        );
        assert_eq!(tail, ConvTail::default());
    }

    /// Purpose: Check that a same-size rewrite replaces the cached messages.
    /// Input: A temporary hub with one message and a cached tail.
    /// Output: None. The test fails if the old message remains.
    #[test]
    fn a_same_size_in_place_rewrite_resets_the_tail() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        append_raw(&hub, "#general", line("#general", "old").as_bytes());
        let mut tail = ConvTail::default();
        hub.read_tail("#general", &mut tail).unwrap();
        let path = hub.conv_path("#general").unwrap();
        let replacement = line("#general", "new");
        assert_eq!(replacement.len() as u64, tail.stamp.len);
        fs::write(&path, replacement).unwrap();
        File::open(&path)
            .unwrap()
            .set_modified(tail.stamp.modified.unwrap() + std::time::Duration::from_secs(1))
            .unwrap();

        let TailRead::Reset(messages) = hub.read_tail("#general", &mut tail).unwrap() else {
            panic!("a same-size rewrite must replace the cached messages");
        };
        assert_eq!(texts(&messages), ["new"]);
        assert_eq!(tail.message_count, 1);
        assert_eq!(
            hub.read_tail("#general", &mut tail).unwrap(),
            TailRead::Unchanged
        );
    }

    /// Purpose: Check that file identity detects replacements of equal or greater size.
    /// Input: A temporary hub with a cached tail and a replacement file.
    /// Output: None. The test fails if the previous file's messages remain.
    #[cfg(unix)]
    #[test]
    fn a_new_file_identity_resets_same_size_and_longer_replacements() {
        for extra in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let hub = Hub::create(root.path(), "h").unwrap();
            append_raw(&hub, "#general", line("#general", "old").as_bytes());
            let mut tail = ConvTail::default();
            hub.read_tail("#general", &mut tail).unwrap();
            let path = hub.conv_path("#general").unwrap();
            let replacement_path = path.with_extension("replacement");
            // Keep the old file open so its inode cannot be reused.
            let _original = File::open(&path).unwrap();
            let mut replacement = line("#general", "new");
            assert_eq!(replacement.len() as u64, tail.byte_offset);
            if extra {
                replacement.push_str(&line("#general", "extra"));
            }
            fs::write(&replacement_path, replacement).unwrap();
            File::open(&replacement_path)
                .unwrap()
                .set_modified(tail.stamp.modified.unwrap())
                .unwrap();
            fs::rename(&replacement_path, &path).unwrap();

            let TailRead::Reset(messages) = hub.read_tail("#general", &mut tail).unwrap() else {
                panic!("a replacement must discard the previous file's messages");
            };
            let expected = if extra {
                vec!["new", "extra"]
            } else {
                vec!["new"]
            };
            assert_eq!(texts(&messages), expected);
            assert_eq!(tail.message_count, expected.len());
        }
    }

    #[test]
    fn legacy_cursors_load_without_offsets_and_gain_them_on_save() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        for text in ["a", "b", "c"] {
            hub.append(&message("#general", "x", text)).unwrap();
        }
        let legacy = r##"{"read": {"#general": 2}, "notified": {"#general": 3}}"##;
        fs::write(hub.cursor_path("me"), legacy).unwrap();
        let mut cursors = hub.cursors("me").unwrap();
        assert_eq!(cursors.read["#general"], 2);
        assert_eq!(cursors.notified["#general"], 3);
        assert_eq!(cursors.read_offset("#general"), None);
        assert_eq!(cursors.notified_offset("#general"), None);
        assert_eq!(
            cursors.read_offset("#other").map(|offset| offset.bytes),
            Some(0)
        );

        let slice = hub
            .messages_from_offset("#general", 2, cursors.read_offset("#general"), None)
            .unwrap();
        assert_eq!(texts(&slice.messages), vec!["c"]);
        let after_two = hub
            .messages_from_offset("#general", 0, None, Some(2))
            .unwrap()
            .next_byte_offset;
        cursors.set_read("#general", 2, after_two);
        let _cursor_lock = hub.lock_cursors("me").unwrap();
        hub.save_cursors("me", &cursors).unwrap();
        let saved = hub.cursors("me").unwrap();
        assert_eq!(saved.read["#general"], 2);
        let offset = saved.read_offset("#general");
        let seeked = hub
            .messages_from_offset("#general", 2, offset, None)
            .unwrap();
        assert_eq!(seeked.messages, slice.messages);

        // A count that moved without its offset makes the offset unknown.
        let mut moved = saved.clone();
        moved.read.insert("#general".to_string(), 1);
        assert_eq!(moved.read_offset("#general"), None);
        moved.set_read("#general", 3, None);
        assert!(!moved.read_bytes.contains_key("#general"));
    }

    /// Purpose: Check that persisted offsets cannot skip replacement messages.
    /// Input: A cursor whose old byte offset spans two replacement messages.
    /// Output: None. The test fails if either read cursor skips the second message.
    #[test]
    fn persisted_offsets_scan_replaced_or_truncated_files_by_line_count() {
        for mode in ["truncate", "replace", "legacy"] {
            let root = tempfile::tempdir().unwrap();
            let hub = Hub::create(root.path(), "h").unwrap();
            let short = line("#general", "new");
            let mut old_message = message("#general", "a", "old");
            old_message.text.clear();
            let overhead = serde_json::to_string(&old_message).unwrap().len() + 1;
            old_message.text = "x".repeat(2 * short.len() - overhead);
            let old = format!("{}\n", serde_json::to_string(&old_message).unwrap());
            assert_eq!(old.len(), 2 * short.len());
            append_raw(&hub, "#general", old.as_bytes());
            append_raw(&hub, "#general", old.as_bytes());
            let first = hub
                .messages_from_offset("#general", 0, None, Some(1))
                .unwrap();
            let mut cursors = Cursors::default();
            cursors.set_read("#general", 1, first.next_byte_offset);
            cursors.set_notified("#general", 1, first.next_byte_offset);
            let mut saved = serde_json::to_value(&cursors).unwrap();
            if mode == "legacy" {
                for key in ["read_bytes", "notified_bytes"] {
                    saved[key]["#general"]
                        .as_object_mut()
                        .unwrap()
                        .remove("stamp");
                }
            }
            let cursors: Cursors = serde_json::from_value(saved).unwrap();
            let path = hub.conv_path("#general").unwrap();
            let _original = File::open(&path).unwrap();
            let replacement = if mode == "replace" {
                path.with_extension("replacement")
            } else {
                path.clone()
            };
            fs::write(&replacement, short.repeat(3)).unwrap();
            File::open(&replacement)
                .unwrap()
                .set_modified(first.stamp.modified.unwrap() + std::time::Duration::from_secs(1))
                .unwrap();
            if mode == "replace" {
                fs::rename(&replacement, &path).unwrap();
            }
            for offset in [
                cursors.read_offset("#general"),
                cursors.notified_offset("#general"),
            ] {
                let slice = hub
                    .messages_from_offset("#general", 1, offset, None)
                    .unwrap();
                assert_eq!(texts(&slice.messages), ["new", "new"], "{mode}");
            }
        }
    }

    #[test]
    fn concurrent_atomic_writes_of_one_process_do_not_collide() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("value.json");
        let mut workers = Vec::new();
        for worker in 0..8u64 {
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                for step in 0..50u64 {
                    let value = BTreeMap::from([("n".to_string(), worker * 1000 + step)]);
                    write_json_atomic(&path, &value).unwrap();
                    let read: BTreeMap<String, u64> = read_json(&path).unwrap();
                    assert!(read.contains_key("n"));
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let names: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["value.json"], "no temp file is left");
    }

    #[test]
    fn member_token_files_add_to_the_legacy_hub_counts() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        hub.update_meta(|meta| {
            meta.tokens.insert("#general".to_string(), 100);
        })
        .unwrap();
        let costs = BTreeMap::from([("#general".to_string(), 5), (DM_TOKENS_KEY.to_string(), 7)]);
        hub.add_member_tokens("a/b", &costs).unwrap();
        hub.add_member_tokens("a/b", &costs).unwrap();
        hub.add_member_tokens("c", &costs).unwrap();
        hub.add_member_tokens("d", &BTreeMap::new()).unwrap();
        let meta = hub.meta().unwrap();
        assert_eq!(meta.tokens["#general"], 100, "hub.json does not change");
        let tokens = hub.tokens(&meta).unwrap();
        assert_eq!(tokens["#general"], 115);
        assert_eq!(tokens[DM_TOKENS_KEY], 21);
    }

    #[test]
    fn file_stamps_change_on_append_and_new_conversation() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "h").unwrap();
        let empty = hub.file_stamps();
        assert_eq!(empty.len(), 1, "only hub.json");
        hub.append(&message("#general", "a", "one")).unwrap();
        let one = hub.file_stamps();
        assert_ne!(one, empty);
        assert_eq!(hub.file_stamps(), one);
        hub.append(&message("#general", "a", "two")).unwrap();
        assert_ne!(hub.file_stamps(), one);
    }

    #[test]
    fn membership_and_suggestions() {
        let root = tempfile::tempdir().unwrap();
        let hub = Hub::create(root.path(), "api-rewrite").unwrap();
        hub.update_meta(|meta| meta.members.push(agent("backend")))
            .unwrap();
        assert_eq!(
            find_membership(root.path(), "sid-backend"),
            Some(("api-rewrite".into(), "backend".into()))
        );
        assert_eq!(find_membership(root.path(), "nobody"), None);
        assert_eq!(
            suggest_hub_name(root.path(), "api-rewirte"),
            Some("api-rewrite".into())
        );
        assert_eq!(suggest_hub_name(root.path(), "zzzzzzzz"), None);
    }

    #[test]
    fn handles_mentions_and_uniqueness() {
        assert_eq!(normalize_handle("@backend"), Some("backend".into()));
        assert_eq!(normalize_handle("@a b"), None);
        assert_eq!(normalize_channel("#tasks"), Some("tasks".into()));

        let members = vec![agent("backend"), agent("backend/scout")];
        assert_eq!(
            parse_mentions("@backend/scout and @backend, see @here. @nobody", &members),
            vec!["backend/scout", "backend", "here"]
        );

        let meta = HubMeta {
            name: "h".into(),
            created_ms: 0,
            archived: false,
            channels: Vec::new(),
            members,
            agent_wake_streak: 0,
            tokens: BTreeMap::new(),
            person_posts_alert_all: true,
        };
        assert_eq!(unique_handle(&meta, "backend", Some("other")), "backend-2");
        assert_eq!(
            unique_handle(&meta, "backend", Some("sid-backend")),
            "backend"
        );
        assert_eq!(unique_handle(&meta, "frontend", None), "frontend");
    }

    #[test]
    fn mentions_preserve_registered_trailing_periods() {
        let members = vec![agent("worker"), agent("worker."), agent("worker..")];
        for (text, expected) in [
            ("@worker", "worker"),
            ("@worker.", "worker."),
            ("@worker.!", "worker."),
            ("@worker!.", "worker"),
            ("@worker..", "worker.."),
            ("@worker...", "worker.."),
        ] {
            assert_eq!(parse_mentions(text, &members), vec![expected]);
        }
        assert_eq!(
            parse_mentions("@worker.", &[agent("worker")]),
            vec!["worker"]
        );
        assert_eq!(parse_mentions("@here.", &members), vec![HERE_MENTION]);
        assert!(parse_mentions("@unknown.", &members).is_empty());
    }

    #[test]
    fn prune_removes_only_old_unused_session_hubs() {
        let root = tempfile::tempdir().unwrap();
        let age = |id: &str, created_ms: u64| {
            let hub = Hub::open_session_hub(root.path(), id).unwrap();
            hub.update_meta(|meta| meta.created_ms = created_ms)
                .unwrap();
            hub
        };
        age("old-empty", 0);
        let old_with_message = age("old-message", 0);
        old_with_message
            .append(&message("#general", "Main", "note"))
            .unwrap();
        age("new-empty", now_ms());
        age("me", 0);

        assert_eq!(prune_idle_session_hubs(root.path(), "me").unwrap(), 1);
        let left = |id: &str| root.path().join(session_hub_name(id)).exists();
        assert!(!left("old-empty"));
        assert!(left("old-message"));
        assert!(left("new-empty"));
        assert!(left("me"));
    }

    #[test]
    fn prune_rechecks_a_hub_that_changes_while_its_lock_is_held() {
        for change in ["member", "message"] {
            let root = tempfile::tempdir().unwrap();
            let hub = Hub::open_session_hub(root.path(), "old").unwrap();
            hub.update_meta(|meta| meta.created_ms = 0).unwrap();
            assert!(is_idle_session_hub(&hub));
            let lock = hub.lock().unwrap();
            let worker_root = root.path().to_path_buf();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let removed = prune_idle_session_hubs(&worker_root, "other").unwrap();
                done_tx.send(removed).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .is_err()
            );
            if change == "member" {
                let mut meta = hub.meta().unwrap();
                meta.members.push(agent("resumed"));
                write_json_atomic(&hub.dir.join(META_FILE), &meta).unwrap();
            } else {
                let path = hub.conv_path("#general").unwrap();
                let line = serde_json::to_string(&message("#general", "a", "keep me")).unwrap();
                fs::write(path, format!("{line}\n")).unwrap();
            }
            drop(lock);
            assert_eq!(
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                0
            );
            worker.join().unwrap();
            assert!(hub.dir.exists(), "{change} keeps the hub alive");
        }
    }

    #[test]
    fn old_member_metadata_keeps_process_based_presence() {
        let mut value = serde_json::to_value(agent("worker")).unwrap();
        value.as_object_mut().unwrap().remove("disconnected");
        let member: Member = serde_json::from_value(value).unwrap();
        assert!(!member.disconnected);
        assert!(member.is_live());
    }

    #[test]
    fn member_of_an_ended_process_is_not_live() {
        let mut member = agent("backend");
        member.pid = Some(std::process::id());
        assert!(member.is_live());
        // No OS gives out a pid this large, so no process has it.
        member.pid = Some(i32::MAX as u32);
        assert!(!member.is_live());

        let meta = HubMeta {
            name: "h".into(),
            created_ms: 0,
            archived: false,
            channels: Vec::new(),
            members: vec![member],
            agent_wake_streak: 0,
            tokens: BTreeMap::new(),
            person_posts_alert_all: true,
        };
        assert_eq!(unique_handle(&meta, "backend", Some("other")), "backend");
    }
}
