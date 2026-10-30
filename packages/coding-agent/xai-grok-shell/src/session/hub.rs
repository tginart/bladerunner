//! Session side of agent hubs: connect at startup, inherit on fork, `/hub`.
//!
//! The hub data and the `agent_hub` tool live in
//! `xai_grok_tools::implementations::grok_build::agent_hub`. This module
//! decides which hub a session is in and runs the person-facing `/hub` command.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use xai_grok_tools::implementations::grok_build::agent_hub::{
    AgentBus, HubView, MemberKind, store,
};

use crate::util::grok_home::grok_home;

const USAGE: &str = "Usage: /hub [list | new <name> [as @handle] | join <name> [as @handle] | leave | open [#channel] | channel <#name> [purpose] | post <#channel|@handle> <text> [--image <path>]... | archive <name>]";
/// Messages that `/hub open` shows.
const FEED_MESSAGES: usize = 30;

pub fn hubs_root() -> PathBuf {
    grok_home().join(store::HUBS_DIR)
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
            joined_ms: store::now_ms(),
            pid: Some(std::process::id()),
        });
        handle
    })?;
    Ok(Some((hub_name, handle)))
}

/// Purpose: Run one `/hub` command for the person at the keyboard.
/// Input: The session's bus, the command arguments, and the default handle.
/// Output: The text to show the person.
pub fn run_command(bus: &AgentBus, args: &str, default_handle: &str) -> String {
    match dispatch(bus, args.trim(), default_handle) {
        Ok(text) => text,
        Err(e) => format!("/hub: {e}"),
    }
}

/// Purpose: Build the status-bar label of a session's hub.
/// Input: The session id.
/// Output: `hub <name> · @<handle>`, plus ` · N unread` when there are unread
///     messages, or `None` when the session is in no named hub.
pub fn status_label(session_id: &str) -> Option<String> {
    status_label_in(&hubs_root(), session_id)
}

fn status_label_in(root: &Path, session_id: &str) -> Option<String> {
    let (name, handle) = store::find_membership(root, session_id)?;
    let hub = store::Hub::open(root, &name).ok()?;
    let unread = unread_count(&hub, &handle).ok()?;
    let mut label = format!("hub {name} \u{b7} @{handle}");
    if unread > 0 {
        label.push_str(&format!(" \u{b7} {unread} unread"));
    }
    Some(label)
}

/// Messages after the member's read cursor, over all channels and its DMs.
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
        unread += hub.messages(&conv, read)?.len();
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
        "new" | "join" => {
            // `join --create` (from `--hub-create`) makes the hub when it is missing.
            let create_if_missing =
                command == "join" && rest.split_whitespace().any(|w| w == "--create");
            let rest = rest.replace("--create", "");
            let (name, handle) = parse_name_and_handle(&rest)?;
            let exists = store::Hub::open(&bus.hubs_root(), &name).is_ok();
            let create = command == "new" || (create_if_missing && !exists);
            let outcome = bus.join(&name, handle.as_deref(), default_handle, create)?;
            let verb = if create {
                "Created and joined"
            } else {
                "Joined"
            };
            Ok(format!(
                "{verb} hub {} as @{}. Subagents of this session join as @{}/<type>. Agents here can now reach each other with the agent_hub tool.",
                outcome.hub, outcome.handle, outcome.handle
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
            require_named_hub(bus)?;
            let name = bus.create_channel(channel, purpose)?;
            Ok(format!("Created #{name}."))
        }
        "open" => feed(bus, rest),
        "post" => {
            let (target, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let (text, images) = split_image_flags(text)?;
            if target.is_empty() || text.trim().is_empty() {
                bail!("{USAGE}");
            }
            require_named_hub(bus)?;
            let person = store::person_handle();
            match bus
                .post_as_person(&person, target, &text, &images)
                .into_result()
            {
                Ok(id) => Ok(format!(
                    "Posted to {target} as @{person} (person), id={id}."
                )),
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
                "Archived hub {rest}. It is now read-only and hidden from /hub list."
            ))
        }
        _ => bail!("{USAGE}"),
    }
}

fn status(bus: &AgentBus) -> Result<String> {
    if bus.is_session_hub() {
        let hubs = list(&bus.hubs_root(), false)?;
        return Ok(format!(
            "Not in a hub. Main and its subagents share a private hub.\n{hubs}\nUse /hub join <name> or /hub new <name>."
        ));
    }
    Ok(format_view(&bus.view(
        xai_grok_tools::implementations::grok_build::MAIN_PEER_ID,
    )?))
}

fn format_view(view: &HubView) -> String {
    let mut out = format!("Hub {} · you are @{}", view.hub, view.handle);
    let members: Vec<String> = view
        .members
        .iter()
        .map(|m| {
            let kind = match m.kind {
                MemberKind::Person => "person".to_string(),
                MemberKind::Agent => m.label.clone(),
            };
            let gone = if m.is_live() { "" } else { ", gone" };
            format!("@{} ({kind}{gone})", m.handle)
        })
        .collect();
    out.push_str(&format!("\n\nMembers: {}", members.join(", ")));
    out.push_str("\n\nChannels:\n");
    let mut total_tokens = view.dm_tokens;
    for c in &view.channels {
        total_tokens += c.tokens;
        out.push_str(&format!(
            "\n- #{}: {} messages, {} unread, ~{} tokens. {}",
            c.name, c.messages, c.unread, c.tokens, c.purpose
        ));
    }
    out.push_str(&format!(
        "\n\nTokens put into agents' context: ~{total_tokens} (DMs ~{}).",
        view.dm_tokens
    ));
    if view.paused {
        out.push_str(
            "\n\nAgent wakes are paused by the loop guard. Any post by a person clears it.",
        );
    }
    out
}

/// Purpose: Show the latest messages of the hub, or of one channel.
/// Input: The bus and an optional `#channel`.
/// Output: Up to FEED_MESSAGES lines, oldest first. Read cursors do not move.
fn feed(bus: &AgentBus, channel: &str) -> Result<String> {
    let hub = bus.hub();
    let meta = hub.meta()?;
    let filter = (!channel.is_empty()).then_some(channel);
    let messages = store::recent_messages(&hub, filter, FEED_MESSAGES)?;
    let title = if channel.is_empty() {
        format!("Hub {}", meta.name)
    } else {
        format!("Hub {} #{}", meta.name, channel.trim_start_matches('#'))
    };
    if messages.is_empty() {
        return Ok(format!("{title}: no messages yet."));
    }
    let mut out = format!("{title}, latest {} message(s):\n", messages.len());
    for m in &messages {
        let place = match store::split_dm_conv(&m.conv) {
            Some((a, b)) => format!("DM @{a}/@{b}"),
            None => m.conv.clone(),
        };
        let time = chrono::DateTime::from_timestamp_millis(m.ts_unix_ms as i64)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let images = if m.images.is_empty() {
            String::new()
        } else {
            format!(" [{} image(s)]", m.images.len())
        };
        out.push_str(&format!(
            "\n- {time} {place} @{}: {}{images}",
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
        return Ok("No hubs yet.".to_string());
    }
    let mut out = String::from("Hubs:\n");
    for meta in hubs {
        let active = meta.members.iter().filter(|m| m.is_live()).count();
        let archived = if meta.archived { ", archived" } else { "" };
        out.push_str(&format!(
            "\n- {}: {active} active member(s), {} channel(s){archived}",
            meta.name,
            meta.channels.len()
        ));
    }
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

fn require_named_hub(bus: &AgentBus) -> Result<()> {
    if bus.is_session_hub() {
        bail!("not in a hub; use /hub join <name> or /hub new <name>");
    }
    Ok(())
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
        assert_eq!(out, "Created #tasks.");
        let out = dispatch(&bus, "", "x").unwrap();
        assert!(out.contains("Hub api · you are @backend"), "{out}");
        assert!(out.contains("#tasks"), "{out}");
        assert!(dispatch(&bus, "join api as @a b", "x").is_err());
        let other = bus_in(root.path(), "s2");
        let out = dispatch(&other, "join web --create", "front").unwrap();
        assert!(
            out.contains("Created and joined hub web as @front"),
            "{out}"
        );
        let out = dispatch(&other, "join api --create", "front").unwrap();
        assert!(out.starts_with("Joined hub api as @front"), "{out}");
        assert!(
            dispatch(&bus, "bogus", "x")
                .unwrap_err()
                .to_string()
                .contains("Usage")
        );
        let out = dispatch(&bus, "leave", "x").unwrap();
        assert!(out.contains("Left hub api"), "{out}");
        assert!(dispatch(&bus, "post #general hi", "x").is_err());
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
    fn open_feed_and_post_with_image() {
        let root = tempfile::tempdir().unwrap();
        let bus = bus_in(root.path(), "s1");
        dispatch(&bus, "new team", "lead").unwrap();
        assert!(
            dispatch(&bus, "open", "x")
                .unwrap()
                .contains("no messages yet")
        );
        let png = root.path().join("ui.png");
        std::fs::write(&png, b"png").unwrap();
        let post = format!("post #general look at this --image {}", png.display());
        dispatch(&bus, &post, "x").unwrap();
        let feed = dispatch(&bus, "open #general", "x").unwrap();
        assert!(feed.contains("#general @"), "{feed}");
        assert!(feed.contains(": look at this [1 image(s)]"), "{feed}");
        assert!(dispatch(&bus, "open #nope", "x").is_err());
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
