//! Launch-time unix socket deny list for sandbox profiles.
//!
//! Restricted profiles keep `/run`, `/var`, and the home directory readable
//! (DNS/NSS, tool config). That leaves D-Bus, the systemd private socket, and
//! container-runtime API sockets path-reachable. A message on those sockets
//! runs work outside the sandboxed process tree: `systemd-run --user` starts a
//! unit that Landlock does not cover, and `docker run -v /:/host` reaches the
//! whole host. Profile resolution adds the well-known endpoints that exist at
//! launch to the profile's `deny` list (bwrap bind-over on Linux, Seatbelt on
//! macOS).
//!
//! This is a static, launch-time list: it does not cover sockets created after
//! startup. For container runtimes the session-long guarantee is the per-spawn
//! child network filter ([`crate::child_net`]).

use std::path::PathBuf;

/// Well-known system-wide container-runtime API sockets.
const SYSTEM_RUNTIME_SOCKETS: &[&str] = &[
    "/run/docker.sock",
    "/var/run/docker.sock",
    "/run/podman/podman.sock",
    "/var/run/podman/podman.sock",
    "/run/containerd/containerd.sock",
    "/var/run/containerd/containerd.sock",
];

/// Rootless engines bind under the calling uid's runtime dir, not `/run`.
#[cfg(unix)]
const PER_UID_RUNTIME_SOCKET_SUFFIXES: &[&str] = &[
    "docker.sock",
    "podman/podman.sock",
    "containerd/containerd.sock",
];

/// Docker Desktop binds per-user endpoints under `$HOME/.docker`
/// (`desktop/` on Linux, `run/` on macOS).
const PER_HOME_RUNTIME_SOCKET_SUFFIXES: &[&str] =
    &[".docker/desktop/docker.sock", ".docker/run/docker.sock"];

/// Which socket families a profile denies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SocketDenyPolicy {
    /// System/session D-Bus and systemd private sockets.
    pub dbus: bool,
    /// Docker, podman, and containerd API sockets.
    pub container_runtime: bool,
}

/// Candidate socket paths for `policy`, before existence checks.
pub(crate) fn socket_deny_candidates(policy: SocketDenyPolicy) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if policy.dbus {
        candidates.extend(dbus_socket_candidates());
    }
    if policy.container_runtime {
        candidates.extend(container_runtime_socket_candidates());
    }
    candidates
}

/// Resolve `candidates` to the real paths of the endpoints that exist now.
///
/// Symlinks (e.g. `/var/run` -> `/run`, or Docker Desktop's
/// `/var/run/docker.sock` -> `~/.docker/run/docker.sock`) are followed, so the
/// deny lands on the inode a `connect()` would reach. Missing or unresolvable
/// endpoints are skipped: a path this process can't resolve is not reachable
/// from inside the sandbox either.
pub(crate) fn existing_socket_paths(candidates: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for candidate in candidates {
        let Ok(path) = dunce::canonicalize(&candidate) else {
            continue;
        };
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn dbus_socket_candidates() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/run/dbus/system_bus_socket"),
        PathBuf::from("/var/run/dbus/system_bus_socket"),
        PathBuf::from("/run/systemd/private"),
    ];
    #[cfg(unix)]
    {
        let uid = current_uid();
        paths.push(PathBuf::from(format!("/run/user/{uid}/bus")));
        paths.push(PathBuf::from(format!("/run/user/{uid}/systemd/private")));
    }
    paths
}

fn container_runtime_socket_candidates() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = SYSTEM_RUNTIME_SOCKETS.iter().map(PathBuf::from).collect();
    #[cfg(unix)]
    {
        let uid = current_uid();
        for suffix in PER_UID_RUNTIME_SOCKET_SUFFIXES {
            paths.push(PathBuf::from(format!("/run/user/{uid}/{suffix}")));
        }
    }
    if let Some(home) = dirs::home_dir() {
        for suffix in PER_HOME_RUNTIME_SOCKET_SUFFIXES {
            paths.push(home.join(suffix));
        }
    }
    paths
}

#[cfg(unix)]
fn current_uid() -> libc::uid_t {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: SocketDenyPolicy = SocketDenyPolicy {
        dbus: false,
        container_runtime: false,
    };

    #[test]
    fn empty_policy_has_no_candidates() {
        assert!(socket_deny_candidates(NONE).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn dbus_policy_covers_system_and_session_buses() {
        let candidates = socket_deny_candidates(SocketDenyPolicy { dbus: true, ..NONE });
        let uid = current_uid();
        for expected in [
            "/run/dbus/system_bus_socket".to_string(),
            "/run/systemd/private".to_string(),
            format!("/run/user/{uid}/bus"),
            format!("/run/user/{uid}/systemd/private"),
        ] {
            assert!(
                candidates.contains(&PathBuf::from(&expected)),
                "missing {expected}: {candidates:?}"
            );
        }
        assert!(!candidates.iter().any(|p| p.ends_with("docker.sock")));
    }

    #[test]
    #[cfg(unix)]
    fn container_runtime_policy_covers_rootful_and_rootless_engines() {
        let candidates = socket_deny_candidates(SocketDenyPolicy {
            container_runtime: true,
            ..NONE
        });
        let uid = current_uid();
        for expected in [
            "/var/run/docker.sock".to_string(),
            "/run/podman/podman.sock".to_string(),
            "/run/containerd/containerd.sock".to_string(),
            format!("/run/user/{uid}/docker.sock"),
            format!("/run/user/{uid}/podman/podman.sock"),
        ] {
            assert!(
                candidates.contains(&PathBuf::from(&expected)),
                "missing {expected}: {candidates:?}"
            );
        }
        assert!(!candidates.iter().any(|p| p.ends_with("bus")));
    }

    #[test]
    #[cfg(unix)]
    fn existing_socket_paths_skip_missing_and_follow_symlinks() {
        let dir = std::env::temp_dir().join(format!("grok-sock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real_dir = dunce::canonicalize(&dir).unwrap();
        let socket = real_dir.join("engine.sock");
        let _ = std::fs::remove_file(&socket);
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let link = real_dir.join("link.sock");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&socket, &link).unwrap();

        let paths = existing_socket_paths([real_dir.join("missing.sock"), link, socket.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(paths, vec![socket]);
    }
}
