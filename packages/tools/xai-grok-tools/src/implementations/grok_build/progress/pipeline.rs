//! Purpose: This module follows the output file of a background command and
//! sends a `TaskProgress` notification each time a progress rule gives a new
//! value.

use std::path::PathBuf;
use std::sync::Weak;
use std::time::Duration;

use super::rule::{ProgressRule, ProgressTracker, ProgressValue};
use crate::computer::types::TerminalBackend;
use crate::implementations::grok_build::monitor::tool::read_new_bytes;
use crate::notification::ToolNotificationHandle;
use crate::notification::types::TaskProgress;

/// Interval between two reads of the output file.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Purpose: Start the task that follows one background command.
/// Input: The task ID, the compiled rule, the terminal backend, the
/// notification handle of the session, and the output file of the command.
/// Output: None. The task stops when the command ends or the session drops
/// the terminal backend.
/// Note: The task keeps a `Weak` backend handle. A strong handle would keep
/// the terminal of a closed session alive.
pub(crate) fn spawn_progress_pipeline(
    task_id: String,
    rule: ProgressRule,
    terminal: Weak<dyn TerminalBackend>,
    notification_handle: ToolNotificationHandle,
    output_file: PathBuf,
) {
    tokio::spawn(run_progress_pipeline(
        task_id,
        rule,
        terminal,
        notification_handle,
        output_file,
    ));
}

async fn run_progress_pipeline(
    task_id: String,
    rule: ProgressRule,
    terminal: Weak<dyn TerminalBackend>,
    notification_handle: ToolNotificationHandle,
    output_file: PathBuf,
) {
    let mut tracker = ProgressTracker::new(rule);
    let mut offset: u64 = 0;
    if let Some(initial) = tracker.latest() {
        send_progress(&notification_handle, &task_id, initial);
    }
    loop {
        // `None` means the session dropped the backend: stop.
        let Some(strong) = terminal.upgrade() else {
            break;
        };
        let snapshot = strong.get_task(&task_id).await;
        drop(strong);
        let is_done = snapshot.is_none_or(|s| s.completed);

        let new_bytes = read_new_bytes(&output_file, &mut offset).await;
        let mut changed = None;
        if !new_bytes.is_empty() {
            changed = tracker.push(&String::from_utf8_lossy(&new_bytes));
        }
        if is_done {
            changed = tracker.finish().or(changed);
        }
        if let Some(value) = changed {
            send_progress(&notification_handle, &task_id, value);
        }
        if is_done {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn send_progress(handle: &ToolNotificationHandle, task_id: &str, value: ProgressValue) {
    handle.send_task_progress(TaskProgress {
        task_id: Some(task_id.to_string()),
        completed: value.completed,
        total: value.total,
    });
}
