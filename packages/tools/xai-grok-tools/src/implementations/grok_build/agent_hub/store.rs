//! On-disk hubs for `agent_hub`: members, channels, DMs and read positions.
//!
//! Layout under `<grok home>/hubs/<name>/`:
//! - `hub.json`: the hub name, members, channels and the loop-guard streak.
//! - `channels/<channel>.jsonl` and `dms/<a>__<b>.jsonl`: append-only messages.
//! - `cursors/<handle>.json`: per-member read and notify positions.
//! - `blobs/`: images attached to messages.
//! - `hub.lock`: an advisory lock that every write holds.
//!
//! Every member process reads and appends to the same directory. No daemon is
//! needed: a writer holds `hub.lock`, and a reader reads up to the last full line.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Directory under the grok home that holds every hub.
pub const HUBS_DIR: &str = "hubs";
/// Channel that every hub starts with.
pub const GENERAL_CHANNEL: &str = "general";
/// Mention that wakes every active member of a channel (people only).
pub const HERE_MENTION: &str = "here";
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
    pub joined_ms: u64,
    /// Process that runs the session. A member whose process ended is gone.
    #[serde(default)]
    pub pid: Option<u32>,
}

impl Member {
    /// True when the member is active and its process, if known, still runs.
    pub fn is_live(&self) -> bool {
        self.active && self.pid.is_none_or(is_process_alive)
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursors {
    /// Messages the member has read with `read` or `inbox`.
    #[serde(default)]
    pub read: BTreeMap<String, usize>,
    /// Messages already checked for DMs and mentions to deliver.
    #[serde(default)]
    pub notified: BTreeMap<String, usize>,
}

/// A handle to one hub directory.
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

    pub fn name(&self) -> &str {
        &self.name
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
        let path = self.conv_path(conv)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for (i, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if i < start {
                continue;
            }
            // A line without a closing brace is a write in progress.
            let Ok(message) = serde_json::from_str::<Message>(&line) else {
                break;
            };
            out.push(message);
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

    fn cursor_path(&self, handle: &str) -> PathBuf {
        self.dir
            .join("cursors")
            .join(format!("{}.json", encode_handle(handle)))
    }
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
///     2. Skip it if a member is live or if it holds a message.
///     3. Under the hub lock, remove its directory.
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
        if is_idle_session_hub(&hub) {
            let _lock = hub.lock()?;
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
    if is_free(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| is_free(candidate))
        .expect("an unused handle exists")
}

/// Purpose: Find the members that a message names with `@`.
/// Input: The message text and the hub members.
/// Output: The named handles, plus `here` when the text has `@here`.
/// Note: A handle must be a whole word. `@backend` does not match `@backend/scout`.
pub fn parse_mentions(text: &str, members: &[Member]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')'))
    {
        let Some(raw) = word.strip_prefix('@') else {
            continue;
        };
        let name = raw.trim_end_matches(['.', '!', '?', '\'', '"']);
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
    is_old
        && !meta.members.iter().any(Member::is_live)
        && !has_messages(&hub.dir.join("channels"))
        && !has_messages(&hub.dir.join("dms"))
}

fn has_messages(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.metadata().is_ok_and(|m| m.len() > 0))
}

/// Escape paths and underscores so `__` separates exactly two encoded handles.
fn encode_handle(handle: &str) -> String {
    handle.replace('/', "~").replace('_', "%5F")
}

fn decode_handle(encoded: &str) -> String {
    encoded.replace("%5F", "_").replace('~', "/")
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

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
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
        };
        assert_eq!(unique_handle(&meta, "backend", Some("other")), "backend-2");
        assert_eq!(
            unique_handle(&meta, "backend", Some("sid-backend")),
            "backend"
        );
        assert_eq!(unique_handle(&meta, "frontend", None), "frontend");
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
        };
        assert_eq!(unique_handle(&meta, "backend", Some("other")), "backend");
    }
}
