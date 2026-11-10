//! Prune old versioned binaries in `~/.grok/downloads/` after an install.
//!
//! The prune keeps the current version and one previous version. It also
//! keeps any older binary that a live process still executes.

use std::path::{Path, PathBuf};

use crate::auto_update::STALE_TMP_AGE;

/// Best-effort cleanup of old versioned binaries for a given binary name.
///
/// Mirrors the npm `cleanupOldVersions()` policy: keeps the current version
/// plus one previous version (in case a process is still running the old binary
/// and hasn't fully loaded all pages yet — deleting it on macOS causes SIGKILL
/// because the kernel can no longer verify the code signature).
///
/// `bin_prefix` is the binary name prefix, e.g. `"grok"` or `"grok-pager"`.
/// Files must match `{bin_prefix}-{digit}*` to be considered versioned binaries
/// (this avoids `grok-*` matching `grok-pager-*` or `grok-latest`).
///
/// Temporary/partial files (containing `.tmp`) are deleted only once they
/// are **stale** (mtime older than [`STALE_TMP_AGE`]). A fresh `.tmp` may be
/// a concurrent updater's in-flight download — the same-instant race the
/// lock-free design accepts — and deleting it out from under that updater
/// would make its atomic rename fail.
///
/// A versioned binary that a live process still executes is kept even when
/// the policy would drop it (see [`executable_is_in_use`]).
pub(crate) async fn cleanup_old_downloads(dir: &Path, bin_prefix: &str, current_version: &str) {
    cleanup_old_downloads_with(dir, bin_prefix, current_version, executable_is_in_use).await;
}

/// [`cleanup_old_downloads`] with an injectable in-use check, so tests can
/// mark a binary as live without a real process.
async fn cleanup_old_downloads_with(
    dir: &Path,
    bin_prefix: &str,
    current_version: &str,
    is_in_use: impl Fn(&Path) -> bool,
) {
    let prefix = format!("{}-", bin_prefix);
    let current_semver = match semver::Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: invalid current version '{}': {}",
                current_version,
                e
            );
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: failed to read {}: {}",
                dir.display(),
                e
            );
            return;
        }
    };

    let mut versioned: Vec<(semver::Version, String)> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        // Temp/partial files: sweep only STALE ones. A fresh `.tmp` may be a
        // concurrent updater's in-flight download — deleting it would make
        // that updater's atomic rename fail with ENOENT.
        if name.contains(".tmp") {
            let stale = match entry.metadata().await.and_then(|m| m.modified()) {
                Ok(modified) => std::time::SystemTime::now()
                    .duration_since(modified)
                    .map(|age| age > STALE_TMP_AGE)
                    // Future mtime (clock skew): can't tell — leave it.
                    .unwrap_or(false),
                // Unknown mtime: leave it; it is swept once readable+old.
                Err(_) => false,
            };
            if stale && let Err(e) = tokio::fs::remove_file(entry.path()).await {
                tracing::warn!("failed to remove stale temp file {}: {}", name, e);
            }
            continue;
        }
        // Skip symlinks (e.g. grok-latest).
        if let Ok(ft) = entry.file_type().await
            && ft.is_symlink()
        {
            continue;
        }
        // The suffix after the prefix must start with a digit to be a versioned
        // binary (avoids `grok-latest`, `grok-pager-*` when prefix is `grok`).
        let suffix = &name[prefix.len()..];
        if !suffix.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        // Extract the version portion via the shared parser (handles the
        // internal `grok-0.1.150-macos-aarch64`, pre-release, and npm
        // `grok-0.1.150` layouts — see `version_from_versioned_binary_name`).
        let Some(ver_str) = crate::version::version_from_versioned_binary_name(&name, bin_prefix)
        else {
            continue;
        };
        if let Ok(v) = semver::Version::parse(&ver_str) {
            // Skip the current version — never delete it.
            if v == current_semver {
                continue;
            }
            versioned.push((v, name));
        }
    }

    // Sort descending by version so the newest is first.
    versioned.sort_by(|a, b| b.0.cmp(&a.0));

    // Keep the most recent old version (index 0), delete the rest (index 1+).
    // This matches the npm policy: current + 1 previous.
    for (_, name) in versioned.iter().skip(1) {
        let path = dir.join(name);
        // Same freshness guard as the `.tmp` sweep: a versioned binary
        // written moments ago is likely a concurrent installer's
        // just-renamed download (its symlink swap hasn't happened yet) —
        // deleting it would leave that installer's swap pointing at
        // nothing. Old binaries from previous releases are days old.
        let fresh = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= STALE_TMP_AGE);
        if fresh {
            continue;
        }
        // A long-lived session may still run an old release. Deleting its
        // executable gets it SIGKILLed on macOS at the next page fault.
        if is_in_use(&path) {
            tracing::debug!(
                path = %path.display(),
                "keeping old binary that a live process still executes"
            );
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!("failed to remove old binary {}: {}", name, e);
        }
    }
}

/// Purpose: This function tells if a live process executes the binary at `path`.
/// Input: The path of a versioned binary.
/// Output: True if a process executes the binary, or if macOS cannot tell.
/// Note: On macOS, a failed process scan returns true (fail closed), because
/// macOS kills a process whose executable is deleted. Linux keeps a deleted
/// executable mapped, so a failed scan there returns false.
pub(crate) fn executable_is_in_use(path: &Path) -> bool {
    match any_process_executes(path) {
        Ok(in_use) => in_use,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not list the processes that execute this binary"
            );
            cfg!(target_os = "macos")
        }
    }
}

fn any_process_executes(path: &Path) -> std::io::Result<bool> {
    let target = match ExecutableId::from_path(path) {
        Ok(id) => id,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let running = running_executable_paths()?;
    Ok(running.iter().any(|candidate| target.matches(candidate)))
}

/// The identity of a binary on disk: its canonical path and, on unix, its
/// device and inode. The inode also matches a hard link to the binary.
struct ExecutableId {
    canonical: Option<PathBuf>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

impl ExecutableId {
    fn from_path(path: &Path) -> std::io::Result<Self> {
        let id = Self {
            canonical: canonical_path(path),
            #[cfg(unix)]
            dev_ino: dev_ino(path),
        };
        if !id.has_any_key() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "binary has no canonical path or inode",
            ));
        }
        Ok(id)
    }

    fn has_any_key(&self) -> bool {
        #[cfg(unix)]
        if self.dev_ino.is_some() {
            return true;
        }
        self.canonical.is_some()
    }

    fn matches(&self, candidate: &Path) -> bool {
        if let Some(canonical) = &self.canonical
            && canonical_path(candidate).as_ref() == Some(canonical)
        {
            return true;
        }
        #[cfg(unix)]
        if self.dev_ino.is_some() && self.dev_ino == dev_ino(candidate) {
            return true;
        }
        false
    }
}

// The workspace bans std::fs::canonicalize for its Windows verbatim paths,
// and dunce is optional in this crate. The ban does not apply here: both
// sides of each compare use this function, and Windows lists no processes.
#[allow(clippy::disallowed_methods)]
fn canonical_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

#[cfg(unix)]
fn dev_ino(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

fn running_executable_paths() -> std::io::Result<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        macos_running_executable_paths()
    }
    #[cfg(target_os = "linux")]
    {
        linux_running_executable_paths()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(Vec::new())
    }
}

/// Purpose: This function lists the executable paths of all processes on macOS.
/// Output: The paths, or an error if the process table cannot be read.
/// Method:
///     1. Get the pids from [`macos_list_pids`].
///     2. Get the path of each pid with `proc_pidpath`.
/// Note: A pid list that gives no paths is an error, not "no process".
#[cfg(target_os = "macos")]
fn macos_running_executable_paths() -> std::io::Result<Vec<PathBuf>> {
    let pids = macos_list_pids()?;
    let mut paths = Vec::new();
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        // SAFETY: `buf` has PROC_PIDPATHINFO_MAXSIZE bytes, the size that
        // proc_pidpath requires, and the length argument matches it.
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        // A pid of another user, or a pid that exited after the list.
        if len <= 0 {
            continue;
        }
        if let Some(bytes) = buf.get(..len as usize)
            && let Ok(s) = std::str::from_utf8(bytes)
        {
            paths.push(PathBuf::from(s));
        }
    }
    if paths.is_empty() {
        return Err(std::io::Error::other(
            "process table gave no executable paths",
        ));
    }
    Ok(paths)
}

/// Purpose: This function reads all pids from `proc_listallpids`.
/// Output: The pids, or an error if the list stays truncated.
/// Method:
///     1. Get the pid count with a null buffer.
///     2. Read the pids into a buffer larger than the count.
///     3. A full buffer can be truncated. Then grow the buffer and try
///        again, at most four times.
#[cfg(target_os = "macos")]
fn macos_list_pids() -> std::io::Result<Vec<i32>> {
    // SAFETY: a null buffer with size 0 is the documented size query. It
    // writes nothing.
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut cap = (needed as usize).saturating_mul(2).saturating_add(32);
    for _ in 0..4 {
        let mut buf = vec![0i32; cap];
        let byte_len = buf.len().saturating_mul(std::mem::size_of::<i32>());
        // SAFETY: `buf` holds `byte_len` bytes of i32 slots, and the size
        // argument is that byte length.
        let filled = unsafe {
            libc::proc_listallpids(
                buf.as_mut_ptr().cast(),
                i32::try_from(byte_len).unwrap_or(i32::MAX),
            )
        };
        if filled <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let count = filled as usize;
        if count < buf.len() {
            buf.truncate(count);
            return Ok(buf);
        }
        cap = count.saturating_mul(2).saturating_add(32);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "process list stayed truncated",
    ))
}

#[cfg(target_os = "linux")]
fn linux_running_executable_paths() -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        // One unreadable entry must not fail the whole scan: on Linux a
        // failed scan means "not in use".
        let Ok(entry) = entry else {
            continue;
        };
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid {
            continue;
        }
        // A pid of another user, or a pid that exited after the read_dir.
        if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
            paths.push(exe);
        }
    }
    Ok(paths)
}

#[cfg(test)]
#[path = "cleanup_downloads_tests.rs"]
mod tests;
