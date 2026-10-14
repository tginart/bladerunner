//! Prompt task creation, normal completion, and small deferred task slots.
//!
//! Flow: create the prompt task -> run the session prompt -> send its result.
//! Scope guards clear active-turn state on every exit, including cancellation.
//! Explicit user stop and send-now teardown live in `cancel.rs`; separating that
//! stage keeps task creation independent from the rules for stopping a session.

use super::*;

pub(super) struct TurnSubagentScopeGuard {
    current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    prompt_id: String,
}

impl TurnSubagentScopeGuard {
    pub(super) fn new(
        current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        prompt_id: String,
    ) -> Self {
        Self {
            current_prompt_id,
            prompt_id,
        }
    }
}

impl Drop for TurnSubagentScopeGuard {
    fn drop(&mut self) {
        let mut current_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned");
        if current_prompt_id.as_deref() == Some(self.prompt_id.as_str()) {
            *current_prompt_id = None;
        }
    }
}

/// RAII guard that stores `false` into an `is_turn_active` flag on drop.
/// Guarantees the flag is cleared on all exit paths (early returns, errors, panics).
pub(super) struct TurnActiveGuard(Option<Arc<std::sync::atomic::AtomicBool>>);

impl TurnActiveGuard {
    pub(super) fn activate(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> Self {
        if let Some(f) = flag {
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Self(flag.cloned())
    }
}

impl Drop for TurnActiveGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(crate) struct AgentTask {
    pub(crate) prompt_id: String,
    pub(crate) handle: tokio::task::AbortHandle,
}

impl AgentTask {
    pub(super) fn new_prompt(
        session: Arc<SessionActor>,
        prompt_id: String,
        input: Vec<ContentBlock>,
        prompt_mode: PromptMode,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        client_identifier: Option<String>,
        screen_mode: Option<String>,
        verbatim: bool,
        send_now: bool,
        json_schema: Option<serde_json::Value>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
        persist_ack: Option<oneshot::Sender<()>>,
        parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    ) -> Self {
        let pid = prompt_id.clone();
        Self {
            prompt_id,
            // Box the task future so the large `handle_prompt` state machine
            // is heap-allocated when the LocalSet task is created (not stacked
            // into the spawn frame). Same root cause as the auth_retry_budget
            // stack overflow: composing huge async fns without boxing.
            handle: tokio::task::spawn_local(Box::pin(async move {
                run_task(
                    session.clone(),
                    input,
                    prompt_mode,
                    trace_gcs_config,
                    artifact_tracker,
                    client_identifier,
                    screen_mode,
                    verbatim,
                    send_now,
                    json_schema,
                    pid,
                    completion_tx,
                    persist_ack,
                    parsed_prompt_tx,
                )
                .await
            }))
            .abort_handle(),
        }
    }

    pub(super) fn abort(&self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

/// Holds at most one spawned task; arming a new one aborts the previous.
/// `Cell` interior mutability because `SessionActor` is `!Send` (single-threaded
/// LocalSet). Backs both the deferred user-message prefix (`take`s the handle to
/// await its result) and the idle-notification debounce (`cancel`s it).
pub(crate) struct TaskSlot<T> {
    handle: std::cell::Cell<Option<tokio::task::JoinHandle<T>>>,
}

impl<T> TaskSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            handle: std::cell::Cell::new(None),
        }
    }

    /// Abort any pending task and store the new one.
    pub(crate) fn arm(&self, handle: tokio::task::JoinHandle<T>) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
        self.handle.set(Some(handle));
    }

    /// Take the pending task handle (e.g. to await its result).
    pub(crate) fn take(&self) -> Option<tokio::task::JoinHandle<T>> {
        self.handle.take()
    }

    /// Abort and drop any pending task (e.g. because a new turn started).
    pub(crate) fn cancel(&self) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
    }
}

async fn run_task(
    session: Arc<SessionActor>,
    input: Vec<ContentBlock>,
    prompt_mode: PromptMode,
    trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
    artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
    client_identifier: Option<String>,
    screen_mode: Option<String>,
    verbatim: bool,
    send_now: bool,
    json_schema: Option<serde_json::Value>,
    prompt_id: String,
    completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    persist_ack: Option<oneshot::Sender<()>>,
    parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
) {
    // Pin the large `handle_prompt` future from a thin never-inlined frame
    // (see turn.rs `box_turn_future` / docs/test-isolation.md §8.3).
    let result = super::turn::box_turn_future(|| {
        session.handle_prompt(
            &prompt_id,
            input,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier,
            screen_mode,
            verbatim,
            send_now,
            json_schema,
            persist_ack,
            parsed_prompt_tx,
        )
    })
    .await;
    let _ = completion_tx.send((prompt_id, result));
}

#[cfg(test)]
mod task_slot_tests {
    // Exercises the shared `TaskSlot<T>` primitive that backs both the deferred
    // prefix and the idle-notification debounce: arm / take / cancel / re-arm.
    use super::TaskSlot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Spawn a 60s task that bumps `fired` by `by`, armed into `slot`.
    fn arm_counter(slot: &TaskSlot<()>, fired: &Arc<AtomicUsize>, by: usize) {
        let f = Arc::clone(fired);
        slot.arm(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            f.fetch_add(by, Ordering::SeqCst);
        }));
    }

    /// A task left armed runs to completion once its delay elapses.
    #[tokio::test(start_paused = true)]
    async fn armed_task_fires_after_delay() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;

        assert_eq!(fired.load(Ordering::SeqCst), 1, "armed task must fire");
    }

    /// `cancel()` aborts a still-pending task (the new-user-prompt reset path).
    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_pending_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        slot.cancel();
        assert!(slot.take().is_none(), "cancel must clear the slot");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "cancelled task must not fire"
        );
    }

    /// Arming a new task aborts the previous one so only the latest fires.
    #[tokio::test(start_paused = true)]
    async fn rearm_aborts_previous_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);
        arm_counter(&slot, &fired, 10);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the re-armed task fires"
        );
    }
}
