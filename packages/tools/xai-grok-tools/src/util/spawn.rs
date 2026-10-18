//! Purpose: Configure child process lifetimes and reap killed search processes.
//! Input: Child processes and commands.
//! Output: Detached commands and bounded child exit status.
//! Note: The process primitives come from the lightweight xai_tty_utils crate.

pub use xai_tty_utils::{
    ProcessGroup, ProcessScope, detach_command, detach_search_command, global_process_scope,
    new_process_group,
};

/// Purpose: Reap a killed search process without an unbounded wait.
/// Input: The killed child process.
/// Output: The exit status, or None after a timeout or wait error.
/// Note: Tokio reaps a child later if this bounded wait cannot reap it.
pub async fn reap_killed_search_child(
    child: &mut tokio::process::Child,
) -> Option<std::process::ExitStatus> {
    let status = xai_tty_utils::reap_killed_bounded(child, xai_tty_utils::KILL_REAP_TIMEOUT).await;
    if status.is_none() {
        tracing::warn!(
            reap_timeout_secs = xai_tty_utils::KILL_REAP_TIMEOUT.as_secs(),
            "killed search child not reaped (bound expired — likely uninterruptible kernel I/O — or wait failed); abandoning to the orphan reaper"
        );
    }
    status
}
