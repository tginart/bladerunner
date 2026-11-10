//! Atomic file writes, shared by the managed-cache marker, the signature
//! sidecar, permission state, and downstream identifier caches (e.g. the
//! telemetry agent id).
//!
//! Hardening (ported from xai-org/grok-build 2bdd1d6):
//! - [`write_atomically`] binds the destination and its parent directory
//!   (dev/inode) before the temp write and proves them unchanged right before
//!   the rename.
//! - [`resolve_write_target`] refuses to follow a symlink that another user owns
//!   in a sticky, world-writable directory (Linux `protected_symlinks` rule,
//!   applied on every unix).
//! - With `mode = None`, an existing destination keeps its permission bits.

use std::io;
use std::path::{Path, PathBuf};

/// Resolve a symlink at the final path component so an atomic temp + rename
/// updates the symlink target instead of replacing the link itself.
///
/// Parent-directory symlinks keep their normal filesystem semantics. Only the
/// final component is followed, including relative links and short link chains.
/// A link that another user owns in a sticky, world-writable directory (such as
/// `/tmp`) is refused with `PermissionDenied`.
pub fn resolve_write_target(path: &Path) -> io::Result<PathBuf> {
    const MAX_SYMLINKS: usize = 40;
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINKS {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                refuse_protected_sticky_symlink(&current, &metadata)?;
                let link = std::fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    current
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(link)
                };
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("too many symlinks while resolving {}", path.display()),
    ))
}

/// Purpose: This function writes a file atomically with a temp file and a rename.
/// Input: The destination path. The file contents. An optional unix mode.
/// Output: `Ok(())` when the new contents are in place.
/// Method:
///     1. Record the identity (dev/inode) of the destination and its parent.
///     2. Write the contents to a unique temp file in the same directory and fsync it.
///     3. Set the temp mode: `mode` if given, else the mode of the existing file.
///     4. Make sure that the destination and parent identities did not change.
///     5. Rename the temp file over the destination.
/// Note: The rename replaces a symlink at the destination; it does not follow it.
/// Callers that must follow a link call [`resolve_write_target`] first.
/// Note: If step 4 finds a change, the function removes the temp file and returns
/// an `InvalidInput` error. The destination is not changed.
pub fn write_atomically(final_path: &Path, contents: &str, mode: Option<u32>) -> io::Result<()> {
    let bound = BoundSlot::bind(final_path)?;
    write_via_temp(final_path, contents, mode, |tmp| {
        if mode.is_none() {
            apply_current_dest_mode(tmp, final_path)?;
        }
        #[cfg(test)]
        tests::run_before_rename_hook(final_path);
        bound.prove_unchanged()?;
        std::fs::rename(tmp, final_path)
    })
}

/// Identity of a path that does not follow a final symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    Absent,
    #[cfg(unix)]
    Present {
        dev: u64,
        ino: u64,
        is_symlink: bool,
    },
    #[cfg(not(unix))]
    Present,
}

/// The destination and its parent directory, bound before the temp write.
struct BoundSlot {
    path: PathBuf,
    dest: Identity,
    parent: Identity,
}

impl BoundSlot {
    fn bind(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            dest: identity_nofollow(path)?,
            parent: identity_nofollow(&parent_dir(path))?,
        })
    }

    fn prove_unchanged(&self) -> io::Result<()> {
        let parent_now = identity_nofollow(&parent_dir(&self.path))?;
        let dest_now = identity_nofollow(&self.path)?;
        if parent_now == self.parent && dest_now == self.dest {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "destination {} changed before the atomic rename",
                self.path.display()
            ),
        ))
    }
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn identity_nofollow(path: &Path) -> io::Result<Identity> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(identity_from_meta(&meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Identity::Absent),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn identity_from_meta(meta: &std::fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt as _;
    Identity::Present {
        dev: meta.dev(),
        ino: meta.ino(),
        is_symlink: meta.file_type().is_symlink(),
    }
}

#[cfg(not(unix))]
fn identity_from_meta(_meta: &std::fs::Metadata) -> Identity {
    Identity::Present
}

/// Copy the permission bits of an existing destination onto the temp file, so a
/// rewrite with `mode = None` does not reset a private file to the umask default.
fn apply_current_dest_mode(tmp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let Ok(meta) = std::fs::metadata(dest) else {
            return Ok(());
        };
        let mode = meta.permissions().mode() & 0o777;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (tmp, dest);
    Ok(())
}

/// Write `contents` to a unique, fsynced temp next to `final_path`, then call
/// `publish` with the temp path. The temp file is removed if any step fails.
fn write_via_temp(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
    publish: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

    let dir = parent_dir(final_path);
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    // `create_new` and a pid + counter suffix keep concurrent writers apart.
    let tmp = dir.join(format!("{name}.{}.{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(contents.as_bytes())?;
            // fsync before rename so a crash cannot publish an empty or torn file.
            f.sync_all()
        })
        .and_then(|()| set_exact_mode(&tmp, mode))
        .and_then(|()| publish(&tmp));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Set `mode` exactly, so the umask cannot change it.
fn set_exact_mode(tmp: &Path, mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (tmp, mode);
    Ok(())
}

/// Refuse to follow `link` when the Linux `protected_symlinks` rule would.
fn refuse_protected_sticky_symlink(link: &Path, link_meta: &std::fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let dir_meta = std::fs::symlink_metadata(parent_dir(link))?;
        let is_protected = is_protected_sticky_link(
            dir_meta.permissions().mode(),
            dir_meta.uid(),
            link_meta.uid(),
            current_euid(),
        );
        if is_protected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to follow a symlink in a sticky world-writable directory: {}",
                    link.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (link, link_meta);
    Ok(())
}

/// Linux `protected_symlinks`: in a sticky, world-writable directory, follow a
/// link only if the directory owner or the current user owns it.
#[cfg(unix)]
fn is_protected_sticky_link(dir_mode: u32, dir_uid: u32, link_uid: u32, euid: u32) -> bool {
    let is_sticky_world_writable = dir_mode & 0o1002 == 0o1002;
    is_sticky_world_writable && link_uid != dir_uid && link_uid != euid
}

#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    type RenameHook = Option<Box<dyn FnMut(&Path)>>;

    thread_local! {
        static BEFORE_RENAME: RefCell<RenameHook> = const { RefCell::new(None) };
    }

    pub(super) fn run_before_rename_hook(dest: &Path) {
        BEFORE_RENAME.with_borrow_mut(|hook| {
            if let Some(hook) = hook.as_mut() {
                hook(dest);
            }
        });
    }

    fn with_before_rename<R>(hook: impl FnMut(&Path) + 'static, body: impl FnOnce() -> R) -> R {
        BEFORE_RENAME.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
        let result = body();
        BEFORE_RENAME.with_borrow_mut(|slot| *slot = None);
        result
    }

    fn tmp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn resolves_relative_final_symlink_chain() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target_dir = root.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("config.toml");
        std::fs::write(&target, "old").unwrap();
        let middle = root.path().join("middle");
        symlink("target/config.toml", &middle).unwrap();
        let link = root.path().join("config.toml");
        symlink("middle", &link).unwrap();

        assert_eq!(resolve_write_target(&link).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_keeps_ordinary_target_behavior() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("shared.toml");
        std::fs::write(&target, "old").unwrap();
        let link = root.path().join("config.toml");
        symlink("shared.toml", &link).unwrap();

        let write_target = resolve_write_target(&link).unwrap();
        write_atomically(&write_target, "new", Some(0o600)).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_symlink_loops() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        symlink("b", &a).unwrap();
        symlink("a", &b).unwrap();

        let error = resolve_write_target(&a).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn refuses_rename_when_destination_is_replaced_mid_write() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("state.json");
        std::fs::write(&dest, "old").unwrap();

        let err = with_before_rename(
            |dest| {
                std::fs::remove_file(dest).unwrap();
                std::fs::write(dest, "swapped").unwrap();
            },
            || write_atomically(&dest, "new", None).unwrap_err(),
        );

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "swapped");
        assert!(tmp_leftovers(root.path()).is_empty());
    }

    #[test]
    fn refuses_rename_when_absent_destination_appears_mid_write() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("state.json");

        let err = with_before_rename(
            |dest| std::fs::write(dest, "planted").unwrap(),
            || write_atomically(&dest, "new", None).unwrap_err(),
        );

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "planted");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_rename_when_parent_directory_is_swapped_mid_write() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("home");
        std::fs::create_dir(&dir).unwrap();
        let dest = dir.join("state.json");
        std::fs::write(&dest, "old").unwrap();
        let moved = root.path().join("moved");

        let err = with_before_rename(
            move |dest| {
                let dir = dest.parent().unwrap();
                std::fs::rename(dir, &moved).unwrap();
                std::fs::create_dir(dir).unwrap();
            },
            || write_atomically(&dest, "new", None).unwrap_err(),
        );

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!dest.exists());
    }

    #[test]
    fn unchanged_destination_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("state.json");
        std::fs::write(&dest, "old").unwrap();

        write_atomically(&dest, "new", None).unwrap();
        write_atomically(&root.path().join("fresh.json"), "x", None).unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(tmp_leftovers(root.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn mode_none_keeps_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("permissions.json");
        std::fs::write(&dest, "old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_atomically(&dest, "new", None).unwrap();

        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn explicit_mode_wins_over_existing_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("id");
        std::fs::write(&dest, "old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomically(&dest, "new", Some(0o600)).unwrap();

        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn sticky_link_rule_matches_protected_symlinks() {
        let (dir_uid, me, other) = (0, 501, 777);
        // Sticky + world-writable: only the dir owner's or my own link is followed.
        assert!(is_protected_sticky_link(0o41777, dir_uid, other, me));
        assert!(!is_protected_sticky_link(0o41777, dir_uid, me, me));
        assert!(!is_protected_sticky_link(0o41777, dir_uid, dir_uid, me));
        // Not sticky, or not world-writable: no restriction.
        assert!(!is_protected_sticky_link(0o40777, dir_uid, other, me));
        assert!(!is_protected_sticky_link(0o41775, dir_uid, other, me));
    }

    #[cfg(unix)]
    #[test]
    fn own_symlink_in_sticky_directory_still_resolves() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let sticky = root.path().join("sticky");
        std::fs::create_dir(&sticky).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let target = root.path().join("real.toml");
        let link = sticky.join("config.toml");
        symlink(&target, &link).unwrap();

        assert_eq!(resolve_write_target(&link).unwrap(), target);
    }
}
