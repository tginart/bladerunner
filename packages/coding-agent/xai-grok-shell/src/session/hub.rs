//! Session side of agent hubs: connect at startup, inherit on fork, `/hub`.
//!
//! The hub data and the `agent_hub` tool live in
//! `xai_grok_tools::implementations::grok_build::agent_hub`. This module
//! decides which hub a session is in and runs the person-facing `/hub` command.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use xai_grok_tools::implementations::grok_build::agent_hub::{
    AgentBus, HubView, MemberKind, ensure_person, person_handle_in, store,
};

use crate::util::grok_home::grok_home;

const USAGE: &str = "Usage: /hub [list | new <name> [as @handle] | join <name> [as @handle] | leave | open [name] [#channel] | channel <#name> [purpose] | post <#channel|@handle> <text> [--image <path>]... | archive <name>]";
/// Content-chunk meta key that marks a `/hub` reply. Its value is the card title.
pub const HUB_CARD_META_KEY: &str = "hubCard";
/// Messages that `/hub open` shows.
const FEED_MESSAGES: usize = 30;

pub fn hubs_root() -> PathBuf {
    grok_home().join(store::HUBS_DIR)
}

/// Purpose: Disconnect hub presence when the root actor exits or its task is cancelled.
/// Input: The root actor's bus.
/// Output: A guard that preserves membership for resume when it drops.
pub(crate) fn track_presence(bus: AgentBus) -> PresenceGuard {
    PresenceGuard(bus)
}

/// Purpose: Tie hub presence to the root actor's lifetime, not the host process.
/// Input: The root actor's bus.
/// Output: A disconnect operation when the guard drops.
pub(crate) struct PresenceGuard(AgentBus);

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        if let Err(error) = self.0.disconnect() {
            tracing::warn!(%error, "agent_hub: could not disconnect the root actor");
        }
    }
}

/// Purpose: Connect a root session to its hub.
/// Input: The root session id.
/// Output: A bus in the named hub where the session is a member, or in the
///     hidden session hub when it is in no named hub.
/// Note: A session becomes a member through `/hub join`, `/hub new`, or a fork
///     of a member session. This rejoins it on resume.
pub fn connect(session_id: &str) -> Result<AgentBus> {
    connect_in(hubs_root(), session_id)
}

fn connect_in(root: PathBuf, session_id: &str) -> Result<AgentBus> {
    if let Err(e) = store::prune_idle_session_hubs(&root, session_id) {
        tracing::warn!(error = %e, "could not prune idle session hubs");
    }
    let bus = AgentBus::new(root.clone(), session_id)?;
    if let Some((hub, handle)) = store::find_membership(&root, session_id) {
        bus.join(&hub, Some(&handle), &handle, false)?;
    }
    Ok(bus)
}

/// Purpose: Give a forked session its parent's hub membership.
/// Input: The parent and child session ids.
/// Output: The hub name and the child's handle, or `None` when the parent is
///     in no named hub.
/// Note: The child's handle is `<parent handle>-fork`, made unique. n-shot
///     shots do not call this, so they stay independent.
pub fn inherit_membership(
    parent_session_id: &str,
    child_session_id: &str,
) -> Result<Option<(String, String)>> {
    inherit_membership_in(&hubs_root(), parent_session_id, child_session_id)
}

fn inherit_membership_in(
    root: &std::path::Path,
    parent_session_id: &str,
    child_session_id: &str,
) -> Result<Option<(String, String)>> {
    let Some((hub_name, parent_handle)) = store::find_membership(root, parent_session_id) else {
        return Ok(None);
    };
    let hub = store::Hub::open(root, &hub_name)?;
    let handle = hub.update_meta(|meta| {
        let handle = store::unique_handle(
            meta,
            &format!("{parent_handle}-fork"),
            Some(child_session_id),
        );
        meta.members.retain(|m| m.handle != handle);
        meta.members.push(store::Member {
            handle: handle.clone(),
            label: "main".to_string(),
            kind: MemberKind::Agent,
            session_id: Some(child_session_id.to_string()),
            active: true,
            disconnected: false,
            joined_ms: store::now_ms(),
            pid: Some(std::process::id()),
        });
        handle
    })?;
    Ok(Some((hub_name, handle)))
}

/// The reply to one `/hub` command, shown as a hub card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubReply {
    /// The card title: the hub name, `hubs`, or `hub`.
    pub title: String,
    /// The card body, in markdown.
    pub markdown: String,
}

/// Purpose: Run one `/hub` command for the person at the keyboard.
/// Input: The session's bus, the command arguments, and the default handle.
/// Output: The reply to show the person.
pub fn run_command(bus: &AgentBus, args: &str, default_handle: &str) -> HubReply {
    let args = args.trim();
    let markdown = match dispatch(bus, args, default_handle) {
        Ok(text) => text,
        Err(e) => format!("/hub: {e}"),
    };
    let command = args.split_whitespace().next().unwrap_or("");
    let title = if matches!(command, "list" | "ls") {
        "hubs".to_string()
    } else if bus.is_session_hub() {
        "hub".to_string()
    } else {
        bus.hub_name()
    };
    HubReply { title, markdown }
}

/// Purpose: Build the status-bar label of a session's hub.
/// Input: The session id.
/// Output: `hub <name> · @<handle>` for a named hub, or `hub · private` for
///     the session hub, plus ` · N unread` when there are unread messages.
///     `None` when the session is in no named hub and its session hub holds
///     no message.
/// Note: The label of the session hub lets the person open the hub where
///     main and its subagents talk.
pub fn status_label(session_id: &str) -> Option<String> {
    status_label_in(&hubs_root(), session_id)
}

fn status_label_in(root: &Path, session_id: &str) -> Option<String> {
    let Some((name, handle)) = store::find_membership(root, session_id) else {
        return session_hub_label(root, session_id);
    };
    let hub = store::Hub::open(root, &name).ok()?;
    let unread = unread_count(&hub, &handle).ok()?;
    Some(with_unread(format!("hub {name} \u{b7} @{handle}"), unread))
}

fn session_hub_label(root: &Path, session_id: &str) -> Option<String> {
    let hub = store::Hub::open_existing_session_hub(root, session_id).ok()?;
    if !hub.has_messages() {
        return None;
    }
    // The same handle that the hub browser reads and saves the cursors of.
    let person = person_handle_in(&hub.meta().ok()?, &store::person_handle());
    let unread = unread_count(&hub, &person).ok()?;
    Some(with_unread("hub \u{b7} private".to_string(), unread))
}

fn with_unread(mut label: String, unread: usize) -> String {
    if unread > 0 {
        label.push_str(&format!(" \u{b7} {unread} unread"));
    }
    label
}

/// Messages after the member's read cursor, over all channels and its DMs.
/// The member's own messages do not count.
fn unread_count(hub: &store::Hub, handle: &str) -> Result<usize> {
    let meta = hub.meta()?;
    let cursors = hub.cursors(handle)?;
    let mut convs: Vec<String> = meta
        .channels
        .iter()
        .map(|c| format!("#{}", c.name))
        .collect();
    convs.extend(hub.dm_convs_for(handle)?);
    let mut unread = 0;
    for conv in convs {
        let read = cursors.read.get(&conv).copied().unwrap_or(0);
        let messages = hub.messages(&conv, read)?;
        unread += messages.iter().filter(|m| m.from != handle).count();
    }
    Ok(unread)
}

/// Purpose: Load the images of a wake as ACP image content.
/// Input: The image file paths.
/// Output: One base64 image per readable file. Unreadable files are skipped.
pub fn load_wake_images(paths: &[PathBuf]) -> Vec<agent_client_protocol::ImageContent> {
    use base64::Engine as _;
    let mut out = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(path) else {
            tracing::warn!(path = %path.display(), "agent_hub: image missing");
            continue;
        };
        out.push(agent_client_protocol::ImageContent::new(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            xai_grok_tools::implementations::grok_build::agent_hub::image_mime_type(path),
        ));
    }
    out
}

/// A handle made from a directory or session title: `my-repo`, `api`.
pub fn default_handle(name: &str) -> String {
    let mut handle: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    handle = handle.trim_matches('-').to_string();
    handle.truncate(32);
    if handle.is_empty() {
        "agent".to_string()
    } else {
        handle
    }
}

fn dispatch(bus: &AgentBus, args: &str, default_handle: &str) -> Result<String> {
    let (command, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match command {
        "" => status(bus),
        "list" | "ls" => list(&bus.hubs_root(), rest == "--archived"),
        "new" | "create" | "join" => {
            // `join` never makes a hub, so a typo cannot. `new` joins a hub that
            // already exists, so `--hub-create` works on every start.
            let (name, handle) = parse_name_and_handle(rest)?;
            let create = if command == "join" {
                false
            } else {
                match store::Hub::create(&bus.hubs_root(), &name) {
                    Ok(_) => true,
                    Err(error) => {
                        // Another session can create the hub before this one does.
                        store::Hub::open(&bus.hubs_root(), &name).map_err(|_| error)?;
                        false
                    }
                }
            };
            let outcome = bus.join(&name, handle.as_deref(), default_handle, false)?;
            let verb = match (create, command) {
                (true, _) => "Created and joined",
                (false, "join") => "Joined",
                (false, _) => "Hub already exists. Joined",
            };
            Ok(format!(
                "{verb} hub {} as @{}. Subagents of this session join as `@{}/<type>`, and agents here can reach each other with the `message_agents` tool.\n\n{}",
                outcome.hub,
                outcome.handle,
                outcome.handle,
                open_link(&outcome.hub, "Open the hub")
            ))
        }
        "leave" => {
            let left = bus.leave()?;
            Ok(format!("Left hub {left}. Its history stays on disk."))
        }
        "channel" => {
            let (channel, purpose) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if channel.is_empty() {
                bail!("{USAGE}");
            }
            let name = bus.create_channel(channel, purpose)?;
            Ok(format!("Created **#{name}**."))
        }
        "open" => feed(bus, rest),
        "post" => {
            let (target, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let (text, images) = split_image_flags(text)?;
            if target.is_empty() || text.trim().is_empty() {
                bail!("{USAGE}");
            }
            let person = ensure_person(&bus.hub(), &store::person_handle())?;
            match bus
                .post_as_person(&person, target, &text, &images)
                .into_result()
            {
                Ok(_id) => Ok(format!("Posted to **{target}** as @{person} (person).")),
                Err(reason) => bail!("{reason}"),
            }
        }
        "archive" => {
            if rest.is_empty() {
                bail!("{USAGE}");
            }
            let hub = store::Hub::open(&bus.hubs_root(), rest)?;
            hub.update_meta(|meta| meta.archived = true)?;
            Ok(format!(
                "Archived hub **{rest}**. It is now read-only and hidden from `/hub list`."
            ))
        }
        _ => bail!("{USAGE}"),
    }
}

fn status(bus: &AgentBus) -> Result<String> {
    if bus.is_session_hub() {
        let hubs = list(&bus.hubs_root(), false)?;
        return Ok(format!(
            "Not in a named hub. Main and its subagents share a private hub: `/hub open` shows it, and `/hub channel` and `/hub post` work in it.\n\n{hubs}\n\nUse `/hub join <name>` or `/hub new <name>`."
        ));
    }
    Ok(format_view(&bus.view(
        xai_grok_tools::implementations::grok_build::MAIN_PEER_ID,
    )?))
}

fn format_view(view: &HubView) -> String {
    let mut out = format!(
        "You are **@{}** · {} · {}",
        view.handle,
        plural(view.members.len(), "member"),
        plural(view.channels.len(), "channel"),
    );
    out.push_str("\n\n**Members**\n");
    for m in &view.members {
        let kind = match m.kind {
            MemberKind::Person => "person".to_string(),
            MemberKind::Agent => m.label.clone(),
        };
        let state = if m.is_live() { "" } else { " · gone" };
        out.push_str(&format!("\n- **@{}** · {kind}{state}", m.handle));
    }
    out.push_str("\n\n**Channels**\n");
    let mut total_tokens = view.dm_tokens;
    for c in &view.channels {
        total_tokens += c.tokens;
        let purpose = if c.purpose.is_empty() {
            String::new()
        } else {
            format!(" · {}", c.purpose)
        };
        out.push_str(&format!(
            "\n- **#{}** · {}, {} unread{purpose}",
            c.name,
            plural(c.messages, "message"),
            c.unread,
        ));
    }
    out.push_str(&format!(
        "\n\n~{total_tokens} tokens put into agents' context (DMs ~{}). {}",
        view.dm_tokens,
        open_link(&view.hub, "Open the hub")
    ));
    if view.paused {
        out.push_str(
            "\n\nAgent wakes are paused by the loop guard. Any post by a person clears it.",
        );
    }
    out
}

/// A markdown link that opens a hub in the hub browser when clicked.
fn open_link(hub: &str, label: &str) -> String {
    format!("[{label}](</hub open {hub}>)")
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Purpose: Show the latest messages of a hub, or of one of its channels.
/// Input: The bus and the arguments: an optional hub name and an optional `#channel`.
/// Output: Up to FEED_MESSAGES lines, oldest first. Read cursors do not move.
fn feed(bus: &AgentBus, args: &str) -> Result<String> {
    let mut name = None;
    let mut channel = None;
    for word in args.split_whitespace() {
        if word.starts_with('#') {
            channel.get_or_insert(word);
        } else {
            name.get_or_insert(word);
        }
    }
    let hub = match name {
        Some(name) => store::Hub::open(&bus.hubs_root(), name)?,
        None => bus.hub(),
    };
    let meta = hub.meta()?;
    let messages = store::recent_messages(&hub, channel, FEED_MESSAGES)?;
    let place = match channel {
        Some(channel) => format!("**#{}**", channel.trim_start_matches('#')),
        None if meta.is_session_hub() => "this session's private hub".to_string(),
        None => format!("hub **{}**", meta.name),
    };
    if messages.is_empty() {
        return Ok(format!("No messages yet in {place}."));
    }
    let mut out = format!("Latest {} in {place}:\n", plural(messages.len(), "message"));
    for m in &messages {
        let place = match store::split_dm_conv(&m.conv) {
            Some((a, b)) => format!("DM @{a}/@{b}"),
            None => m.conv.clone(),
        };
        let time = chrono::DateTime::from_timestamp_millis(m.ts_unix_ms as i64)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%-I:%M %p")
                    .to_string()
            })
            .unwrap_or_default();
        let images = match m.images.len() {
            0 => String::new(),
            1 => " · 1 image".to_string(),
            n => format!(" · {n} images"),
        };
        out.push_str(&format!(
            "\n- **@{}** in {place} · {time}{images}  \n  {}",
            m.from, m.text
        ));
    }
    Ok(out)
}

/// Split `--image <path>` flags off the text of `/hub post`.
fn split_image_flags(text: &str) -> Result<(String, Vec<PathBuf>)> {
    let mut words = Vec::new();
    let mut images = Vec::new();
    let mut parts = text.split_whitespace();
    while let Some(word) = parts.next() {
        if word != "--image" {
            words.push(word);
            continue;
        }
        let Some(path) = parts.next() else {
            bail!("--image needs a file path");
        };
        images.push(expand_home(path));
    }
    Ok((words.join(" "), images))
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn list(root: &std::path::Path, include_archived: bool) -> Result<String> {
    let hubs = store::list_hubs(root, include_archived)?;
    if hubs.is_empty() {
        return Ok("No hubs yet. Use `/hub new <name>` to make one.".to_string());
    }
    let mut out = String::new();
    for meta in hubs {
        let live = meta.members.iter().filter(|m| m.is_live()).count();
        let archived = if meta.archived { " · archived" } else { "" };
        out.push_str(&format!(
            "- {} · {live} live of {} · {}{archived}\n",
            open_link(&meta.name, &meta.name),
            plural(meta.members.len(), "member"),
            plural(meta.channels.len(), "channel"),
        ));
    }
    out.push_str("\nClick a hub to open it.");
    Ok(out)
}

fn parse_name_and_handle(rest: &str) -> Result<(String, Option<String>)> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    match words.as_slice() {
        [name] => Ok((name.to_string(), None)),
        [name, "as", handle] => Ok((name.to_string(), Some(handle.to_string()))),
        _ => bail!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::implementations::grok_build::MAIN_PEER_ID;

    fn bus_in(root: &std::path::Path, sid: &str) -> AgentBus {
        let bus = AgentBus::new(root.to_path_buf(), sid).unwrap();
        bus.register(MAIN_PEER_ID, "main", None);
        bus
    }

    #[test]
    fn default_handles() {
        assert_eq!(default_handle("My Repo!"), "my-repo");
        assert_eq!(default_handle("***"), "agent");
    }

    #[test]
    fn join_requires_existing_hub_and_new_creates() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        let out = dispatch(&bus, "join api", "backend")
            .unwrap_err()
            .to_string();
        assert!(out.contains("/hub new api"), "{out}");
        let out = dispatch(&bus, "new api", "backend").unwrap();
        assert!(
            out.contains("Created and joined hub api as @backend"),
            "{out}"
        );
        let out = dispatch(&bus, "channel #tasks hand-offs", "x").unwrap();
        assert_eq!(out, "Created **#tasks**.");
        let out = dispatch(&bus, "", "x").unwrap();
        assert!(out.contains("You are **@backend**"), "{out}");
        assert!(out.contains("(</hub open api>)"), "{out}");
        assert!(out.contains("#tasks"), "{out}");
        assert!(dispatch(&bus, "join api as @a b", "x").is_err());
        let other = bus_in(root.path(), "s2");
        let out = dispatch(&other, "create web", "front").unwrap();
        assert!(
            out.contains("Created and joined hub web as @front"),
            "{out}"
        );
        let out = dispatch(&other, "new api", "front").unwrap();
        assert!(
            out.starts_with("Hub already exists. Joined hub api as @front"),
            "{out}"
        );
        assert!(
            dispatch(&bus, "bogus", "x")
                .unwrap_err()
                .to_string()
                .contains("Usage")
        );
        let out = dispatch(&bus, "leave", "x").unwrap();
        assert!(out.contains("Left hub api"), "{out}");
        let out = dispatch(&bus, "", "x").unwrap();
        assert!(out.contains("`/hub open` shows it"), "{out}");
    }

    #[test]
    fn person_can_add_channels_and_post_in_the_session_hub() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        assert!(bus.is_session_hub());
        let out = dispatch(&bus, "channel #plan who does what", "x").unwrap();
        assert_eq!(out, "Created **#plan**.");
        let out = dispatch(&bus, "post #plan @Main split the work", "x").unwrap();
        assert!(out.contains("(person)"), "{out}");
        let inbox = bus.inbox(MAIN_PEER_ID).unwrap();
        assert_eq!(inbox.len(), 1);
        let out = dispatch(&bus, "open", "x").unwrap();
        assert!(out.contains("this session's private hub"), "{out}");
    }

    /// Purpose: Check that concurrent new commands join the same hub without loss of members.
    /// Input: Two sessions with the same requested handle and a shared hub directory.
    /// Output: Both commands succeed, and both sessions retain distinct handles.
    #[test]
    fn concurrent_new_commands_create_or_join() {
        for _ in 0..16 {
            let root = tempfile::tempdir().unwrap();
            let first = bus_in(root.path(), "first");
            let second = bus_in(root.path(), "second");
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    barrier.wait();
                    dispatch(&first, "new team as @worker", "unused").unwrap()
                });
                let second = scope.spawn(|| {
                    barrier.wait();
                    dispatch(&second, "new team as @worker", "unused").unwrap()
                });
                assert!(first.join().unwrap().contains("hub team as @worker"));
                assert!(second.join().unwrap().contains("hub team as @worker"));
            });
            assert_eq!(first.hub_name(), "team");
            assert_eq!(second.hub_name(), "team");
            let meta = first.hub_meta().unwrap();
            assert_eq!(meta.members.len(), 2);
            for session_id in ["first", "second"] {
                assert!(meta.members.iter().any(|member| {
                    member.session_id.as_deref() == Some(session_id) && member.is_live()
                }));
            }
            assert_ne!(meta.members[0].handle, meta.members[1].handle);
        }
    }

    /// Purpose: Check that create-or-join does not hide creation or join errors.
    /// Input: A blocked hub path, an archived hub, and an invalid handle.
    /// Output: Each command reports its error and leaves the session in its private hub.
    #[test]
    fn new_preserves_creation_and_join_errors() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        std::fs::write(root.path().join("blocked"), b"not a directory").unwrap();
        let error = store::Hub::create(root.path(), "blocked")
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            dispatch(&bus, "new blocked", "worker")
                .unwrap_err()
                .to_string(),
            error
        );
        let hub = store::Hub::create(root.path(), "team").unwrap();
        let error = dispatch(&bus, "new team as @bad/handle", "worker")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid handle"), "{error}");
        hub.update_meta(|meta| meta.archived = true).unwrap();
        let error = dispatch(&bus, "create team", "worker")
            .unwrap_err()
            .to_string();
        assert!(error.contains("archived"), "{error}");
        assert!(bus.is_session_hub());
        assert!(hub.meta().unwrap().members.is_empty());
    }

    #[test]
    fn fork_inherits_membership_and_resume_rejoins() {
        let root = tempfile::tempdir().unwrap();
        let parent = bus_in(root.path(), "parent");
        assert_eq!(
            inherit_membership_in(root.path(), "parent", "child").unwrap(),
            None
        );
        dispatch(&parent, "new api", "backend").unwrap();

        let inherited = inherit_membership_in(root.path(), "parent", "child").unwrap();
        assert_eq!(inherited, Some(("api".into(), "backend-fork".into())));

        let child = connect_in(root.path().to_path_buf(), "child").unwrap();
        child.register(MAIN_PEER_ID, "main", None);
        assert_eq!(child.hub_name(), "api");
        assert_eq!(child.view(MAIN_PEER_ID).unwrap().handle, "backend-fork");

        let resumed = connect_in(root.path().to_path_buf(), "parent").unwrap();
        resumed.register(MAIN_PEER_ID, "main", None);
        assert_eq!(resumed.view(MAIN_PEER_ID).unwrap().handle, "backend");

        let stranger = connect_in(root.path().to_path_buf(), "other").unwrap();
        assert!(stranger.is_session_hub());
    }

    #[test]
    fn root_exit_ends_presence_and_resume_restores_membership() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "root");
        dispatch(&bus, "new team", "worker").unwrap();
        let hub = bus.hub();
        let joined_ms = hub.meta().unwrap().member("worker").unwrap().joined_ms;
        let presence = track_presence(bus.clone());
        drop(presence);
        assert!(!hub.meta().unwrap().member("worker").unwrap().is_live());
        let other = bus_in(root.path(), "other");
        dispatch(&other, "join team", "other").unwrap();
        assert!(
            other
                .send(MAIN_PEER_ID, "@worker", "no actor", None, &[])
                .into_result()
                .is_err()
        );
        let resumed = connect_in(root.path().to_path_buf(), "root").unwrap();
        resumed.register(MAIN_PEER_ID, "main", None);
        assert_eq!(resumed.hub_name(), "team");
        assert_eq!(resumed.view(MAIN_PEER_ID).unwrap().handle, "worker");
        let meta = hub.meta().unwrap();
        assert!(meta.member("worker").unwrap().is_live());
        assert_eq!(meta.member("worker").unwrap().joined_ms, joined_ms);
    }

    #[tokio::test]
    async fn cancelled_root_task_disconnects_its_hub_presence() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "root");
        dispatch(&bus, "new team", "worker").unwrap();
        let hub = bus.hub();
        let presence = track_presence(bus);
        let actor = tokio::spawn(async move {
            let _presence = presence;
            std::future::pending::<()>().await;
        });
        actor.abort();
        assert!(actor.await.unwrap_err().is_cancelled());
        assert!(!hub.meta().unwrap().member("worker").unwrap().is_live());
        assert!(store::find_membership(root.path(), "root").is_some());
    }

    #[test]
    fn fork_handles_stay_addressable_at_the_length_limit() {
        let root = tempfile::tempdir().unwrap();
        let parent = bus_in(root.path(), "parent");
        parent
            .join("team", Some(&"p".repeat(64)), "unused", true)
            .unwrap();
        for id in ["first-child", "second-child"] {
            let (_, handle) = inherit_membership_in(root.path(), "parent", id)
                .unwrap()
                .unwrap();
            assert_eq!(store::normalize_handle(&handle), Some(handle.clone()));
            let child = connect_in(root.path().to_path_buf(), id).unwrap();
            child.register(MAIN_PEER_ID, "main", None);
            parent
                .send(MAIN_PEER_ID, &handle, id, None, &[])
                .into_result()
                .unwrap();
            assert_eq!(child.inbox(MAIN_PEER_ID).unwrap()[0].text, id);
        }
    }

    #[test]
    fn open_feed_and_post_with_image() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        dispatch(&bus, "new team", "lead").unwrap();
        assert!(
            dispatch(&bus, "open", "x")
                .unwrap()
                .contains("No messages yet")
        );
        let png = root.path().join("ui.png");
        std::fs::write(&png, b"png").unwrap();
        let post = format!("post #general look at this --image {}", png.display());
        dispatch(&bus, &post, "x").unwrap();
        let feed = dispatch(&bus, "open #general", "x").unwrap();
        assert!(feed.contains("in #general"), "{feed}");
        assert!(feed.contains("· 1 image"), "{feed}");
        assert!(feed.contains("look at this"), "{feed}");
        assert!(dispatch(&bus, "open #nope", "x").is_err());
        let other = dispatch(&bus, "open team #general", "x").unwrap();
        assert_eq!(other, feed, "a named hub opens the same feed");
        assert!(dispatch(&bus, "open nope", "x").is_err());
        let list = list(root.path(), false).unwrap();
        assert!(list.contains("[team](</hub open team>)"), "{list}");
        assert!(dispatch(&bus, "post #general hi --image", "x").is_err());
    }

    #[test]
    fn status_label_counts_unread() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(status_label_in(root.path(), "s1"), None);
        let bus = bus_in(root.path(), "s1");
        dispatch(&bus, "new api", "backend").unwrap();
        assert_eq!(
            status_label_in(root.path(), "s1").as_deref(),
            Some("hub api \u{b7} @backend")
        );
        dispatch(&bus, "post #general hello", "x").unwrap();
        assert_eq!(
            status_label_in(root.path(), "s1").as_deref(),
            Some("hub api \u{b7} @backend \u{b7} 1 unread")
        );
    }

    #[test]
    fn status_label_shows_the_session_hub_once_it_has_messages() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        assert_eq!(status_label_in(root.path(), "s1"), None);
        dispatch(&bus, "post #general hi", "x").unwrap();
        assert_eq!(
            status_label_in(root.path(), "s1").as_deref(),
            Some("hub \u{b7} private")
        );
        bus.send(MAIN_PEER_ID, "#general", "status", None, &[])
            .into_result()
            .unwrap();
        assert_eq!(
            status_label_in(root.path(), "s1").as_deref(),
            Some("hub \u{b7} private \u{b7} 1 unread")
        );
        assert_eq!(status_label_in(root.path(), "s2"), None);
    }

    #[test]
    fn person_post_reaches_agent_inbox() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        dispatch(&bus, "new team", "lead").unwrap();
        let out = dispatch(&bus, "post #general @lead please review", "x").unwrap();
        assert!(out.contains("(person)"), "{out}");
        let inbox = bus.inbox(MAIN_PEER_ID).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].text, "@lead please review");
    }
}
