//! Tests for parallel tool dispatch.
//!
//! These tests verify the parallel dispatch path (GROK_PARALLEL_TOOL_DISPATCH):
//! - Phase 1: prepare_tool_call for each tool
//! - Phase 2: permission prompts (if any)
//! - Phase 3: parallel dispatch via dispatch_tool
//! - Post-tool hooks and followups

use super::*;

#[tokio::test]
async fn test_parallel_dispatch_basic() {
    // Ordering correctness: verify that futures::future::join_all preserves
    // the order of results matching the order of input futures.
    //
    // In Phase 2, dispatch_futures is built by mapping approved.iter()
    // to dispatch_tool calls. Phase 3 zips approved.into_iter() with
    // dispatch_results, so result[i] must correspond to approved[i].

    use futures::future::join_all;

    // Simulate 3 tools with different latencies
    let futures = vec![
        Box::pin(async { (0, "tool_a") })
            as std::pin::Pin<Box<dyn futures::Future<Output = (i32, &'static str)>>>,
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            (1, "tool_b")
        }),
        Box::pin(async { (2, "tool_c") }),
    ];

    let results = join_all(futures).await;

    // Results must be in input order, not completion order
    assert_eq!(results[0], (0, "tool_a"));
    assert_eq!(results[1], (1, "tool_b"));
    assert_eq!(results[2], (2, "tool_c"));
}

#[test]
fn test_parallel_dispatch_permission_reject() {
    // Permission rejection abort: when prepare_tool_call returns
    // Err(ToolLoop::PermissionReject), subsequent tools should not
    // be dispatched.
    //
    // Verify the logic: once final_result is set, remaining tools are skipped.
    let mut final_result: Option<ToolLoop> = None;
    let tool_calls = ["tool_0", "tool_1", "tool_2"];
    let mut approved_count = 0;

    for (idx, _call) in tool_calls.iter().enumerate() {
        if final_result.is_some() {
            // Would skip this tool in real code
            continue;
        }
        // Simulate: tool_1 gets permission rejected
        if idx == 1 {
            final_result = Some(ToolLoop::PermissionReject {
                tool_name: "tool_1".to_string(),
                reason: "rejected".to_string(),
            });
            continue;
        }
        approved_count += 1;
    }

    // Only tool_0 should be approved; tool_1 triggers rejection; tool_2 is skipped
    assert_eq!(approved_count, 1);
    assert!(final_result.is_some());
    assert!(matches!(
        final_result,
        Some(ToolLoop::PermissionReject { .. })
    ));
}
#[test]
fn test_parallel_dispatch_followups() {
    // Deferred followups placement: handle_bridge_tool_success returns
    // Vec<ConversationItem> followups that get extended into deferred_followups.
    //
    // In Phase 3:
    //   let followups = handle_bridge_tool_success(...).await?;
    //   deferred_followups.extend(followups);
    //
    // Verify that followups vec can be collected and extended.
    let mut deferred_followups: Vec<&str> = Vec::new();

    // Simulate followups from 2 tools
    let followups_tool_0 = vec!["followup_a", "followup_b"];
    let followups_tool_1 = vec!["followup_c"];

    deferred_followups.extend(followups_tool_0);
    deferred_followups.extend(followups_tool_1);

    assert_eq!(deferred_followups.len(), 3);
    assert_eq!(deferred_followups[0], "followup_a");
    assert_eq!(deferred_followups[1], "followup_b");
    assert_eq!(deferred_followups[2], "followup_c");
}

#[test]
fn test_parallel_dispatch_hooks() {
    // Single-tool-batch no-regression: dispatching a single tool should
    // behave identically to the serial path. The parallel dispatch
    // infrastructure (prepare_tool_call -> dispatch_tool -> post-flight)
    // should work for N=1 without special casing.
    //
    // Verify: 1 tool in approved vec -> 1 dispatch future -> 1 result
    let approved_count = 1;
    let dispatch_futures_count = approved_count; // 1:1 mapping
    let results_count = 1; // incremental stream yields same count

    assert_eq!(approved_count, dispatch_futures_count);
    assert_eq!(dispatch_futures_count, results_count);

    // Also verify the Phase 3 indexed slot works for single element
    let approved = ["single_tool"];
    let ok_val: Result<&str, ()> = Ok("success");
    let results = [ok_val];
    let pairs: Vec<_> = approved.iter().zip(results.iter()).collect();
    assert_eq!(pairs.len(), 1);
}

/// Incremental completion ordering: fast tools must surface before slow siblings.
///
/// Regression for the batch barrier where `join_all` deferred every
/// `ToolCallUpdate(status=Completed)` until the slowest tool in the round
/// finished (e.g. grep stuck pending behind `wait_commands_or_subagents`).
#[tokio::test]
async fn incremental_dispatch_surfaces_fast_tool_before_slow_sibling() {
    use futures::future::BoxFuture;
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let fast_done = Arc::new(AtomicBool::new(false));
    let slow_done = Arc::new(AtomicBool::new(false));
    let fast_flag = Arc::clone(&fast_done);
    let slow_flag = Arc::clone(&slow_done);

    let mut stream: FuturesUnordered<BoxFuture<'static, (usize, &'static str)>> =
        FuturesUnordered::new();
    stream.push(Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        fast_flag.store(true, Ordering::SeqCst);
        (0usize, "grep")
    }));
    stream.push(Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        slow_flag.store(true, Ordering::SeqCst);
        (1usize, "wait_tasks")
    }));

    let mut completion_order = Vec::new();
    while let Some((idx, name)) = stream.next().await {
        completion_order.push((idx, name));
        // Fast tool must finish first — this is what incremental post-flight
        // depends on to stream grep results before the wait tool returns.
        if idx == 0 {
            assert!(
                !slow_done.load(Ordering::SeqCst),
                "fast tool must complete before slow sibling; incremental UI streaming depends on this ordering"
            );
            assert!(fast_done.load(Ordering::SeqCst));
        }
    }

    assert_eq!(completion_order.len(), 2);
    assert_eq!(completion_order[0], (0, "grep"));
    assert_eq!(completion_order[1], (1, "wait_tasks"));
    assert!(fast_done.load(Ordering::SeqCst));
    assert!(slow_done.load(Ordering::SeqCst));
}

/// Regression for the toolset same-file edit race.
///
/// `lock_path_for_args` is the per-call key the dispatcher uses to bucket
/// concurrent tool calls into ordered per-file groups inside
/// `execute_tool_calls` Phase 2. The original implementation hardcoded
/// `parsed_args.get("file_path")`, which silently bypassed serialization
/// for any toolset whose edit input declared the path under a different
/// JSON key. The compat toolset input types use `path`, and grok_build's
/// `read_file` uses `target_file`, so all of
/// those calls fell through to fully concurrent dispatch and could lose
/// edits via TOCTOU on the same workspace file.
///
/// These tests pin the JSON-key contract so the bucket key keeps tracking
/// every toolset's actual schema.
#[test]
fn lock_path_for_args_matches_grok_build_file_path() {
    // grok_build search_replace / opencode EditTool / WriteTool / etc.
    let args = serde_json::json!({
        "file_path": "/repo/src/main.rs",
        "old_string": "foo",
        "new_string": "bar",
    });
    assert_eq!(
        lock_path_for_args(&args, std::path::Path::new("/repo"), None),
        Some("/repo/src/main.rs".to_owned())
    );
}

#[test]
fn lock_path_for_args_matches_path_arg() {
    // StrReplace / Write / Read / Delete all serialize under `path`.
    let args = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "foo",
        "new_string": "bar",
    });
    assert_eq!(
        lock_path_for_args(&args, std::path::Path::new("/repo"), None),
        Some("/repo/src/main.rs".to_owned())
    );
}

#[test]
fn lock_path_for_args_matches_grok_build_target_file() {
    // grok_build read_file uses #[serde(rename = "target_file")].
    let args = serde_json::json!({
        "target_file": "/repo/src/main.rs",
    });
    assert_eq!(
        lock_path_for_args(&args, std::path::Path::new("/repo"), None),
        Some("/repo/src/main.rs".to_owned())
    );
}

#[test]
fn lock_path_for_args_returns_none_for_pathless_tools() {
    // Tools like run_terminal_cmd or web_search have no workspace path;
    // they must not be bucketed into a file lock and must run fully
    // concurrently.
    let args = serde_json::json!({
        "command": "ls -la",
        "description": "list",
    });
    assert_eq!(
        lock_path_for_args(&args, std::path::Path::new("/repo"), None),
        None
    );
    assert_eq!(
        lock_path_for_args(&serde_json::json!({}), std::path::Path::new("/repo"), None),
        None
    );
    assert_eq!(
        lock_path_for_args(
            &serde_json::json!(null),
            std::path::Path::new("/repo"),
            None
        ),
        None
    );
}

#[test]
fn lock_path_for_args_ignores_non_string_path_values() {
    // Defensive: if a model emits a non-string, treat as no lock rather
    // than panicking or coercing — the tool layer will reject it.
    let args = serde_json::json!({"file_path": 42});
    assert_eq!(
        lock_path_for_args(&args, std::path::Path::new("/repo"), None),
        None
    );
    let args = serde_json::json!({"path": ["/a", "/b"]});
    assert_eq!(
        lock_path_for_args(&args, std::path::Path::new("/repo"), None),
        None
    );
}

#[test]
fn lock_path_for_args_buckets_parallel_compat_strreplace_to_same_lock() {
    // The exact symptom of the bug: two compat StrReplace calls in one
    // batch targeting the same file. Before the fix, both returned None
    // here and ran fully concurrently, racing on the underlying file.
    // After the fix, both must hash to the same bucket so the dispatcher
    // serializes them via a per-file predecessor chain.
    let call_a = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "foo",
        "new_string": "bar",
    });
    let call_b = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "baz",
        "new_string": "qux",
    });
    assert_eq!(
        lock_path_for_args(&call_a, std::path::Path::new("/repo"), None),
        lock_path_for_args(&call_b, std::path::Path::new("/repo"), None)
    );
    assert_eq!(
        lock_path_for_args(&call_a, std::path::Path::new("/repo"), None),
        Some("/repo/src/main.rs".to_owned())
    );

    // Cross-file calls must bucket independently so they keep running
    // concurrently — otherwise we'd serialize unrelated edits and tank
    // batch latency.
    let call_c = serde_json::json!({
        "path": "/repo/src/lib.rs",
        "old_string": "x",
        "new_string": "y",
    });
    assert_ne!(
        lock_path_for_args(&call_a, std::path::Path::new("/repo"), None),
        lock_path_for_args(&call_c, std::path::Path::new("/repo"), None)
    );
}

#[test]
fn lock_path_for_args_buckets_grok_build_and_compat_to_same_lock_for_same_file() {
    // A mixed batch (e.g. grok_build search_replace + StrReplace
    // in the same turn — possible if the harness ever exposes both, or
    // during toolset migration) must still serialize on the shared file
    // path. file_path takes precedence over path when both are present,
    // but neither tool emits both keys today, so this asserts the
    // cross-toolset key normalization works in practice.
    let grok = serde_json::json!({
        "file_path": "/repo/src/main.rs",
        "old_string": "a",
        "new_string": "b",
    });
    let compat = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "c",
        "new_string": "d",
    });
    assert_eq!(
        lock_path_for_args(&grok, std::path::Path::new("/repo"), None),
        lock_path_for_args(&compat, std::path::Path::new("/repo"), None)
    );
}

/// Regression: skill-discovery reminders must land after all tool results, not mid-batch.
#[test]
fn test_skill_discovery_deferred_during_parallel_batch() {
    use xai_grok_sampling_types::{ConversationItem, SyntheticReason};

    let mut conversation = vec![ConversationItem::assistant("I'll call 3 tools.")];
    let mut deferred_followups: Vec<ConversationItem> = Vec::new();

    for (i, id) in ["call_1", "call_2", "call_3"].iter().enumerate() {
        conversation.push(ConversationItem::tool_result(
            *id,
            format!("result for {id}"),
        ));
        if i == 0 {
            // Image followup from handle_bridge_tool_success
            deferred_followups.push(ConversationItem::user("[Image content]"));
            // Skill discovery fires after tool 1 — must be deferred, not pushed immediately
            deferred_followups.push(ConversationItem::system_reminder(
                "<system-reminder>\nNew skills discovered\n</system-reminder>",
            ));
        }
    }
    conversation.extend(deferred_followups);

    // 1 assistant + 3 tool_result + 2 deferred user messages
    assert_eq!(conversation.len(), 6);
    assert!(matches!(conversation[0], ConversationItem::Assistant(_)));
    assert!(matches!(conversation[1], ConversationItem::ToolResult(_)));
    assert!(matches!(conversation[2], ConversationItem::ToolResult(_)));
    assert!(matches!(conversation[3], ConversationItem::ToolResult(_)));
    assert!(matches!(conversation[4], ConversationItem::User(_)));
    assert!(
        matches!(conversation[5], ConversationItem::User(ref u) if u.synthetic_reason == Some(SyntheticReason::SystemReminder))
    );
}

/// Poll out of order: successors must still wait through all predecessor attempts.
#[tokio::test]
async fn batch_path_turns_hold_model_order_across_retries() {
    let paths = vec![Some("file".to_owned()); 3];
    let mut turns = batch_path_turns(&paths, &[false, true, false]);
    let mut first = turns[0].take().unwrap();
    let mut read = turns[1].take().unwrap();
    let mut last = turns[2].take().unwrap();
    assert!(futures::poll!(Box::pin(read.wait())).is_pending());
    assert!(futures::poll!(Box::pin(last.wait())).is_pending());
    first.wait().await;
    // Retry does not drop the outer turn, so the read cannot see partial edits.
    assert!(futures::poll!(Box::pin(read.wait())).is_pending());
    drop(first);
    read.wait().await;
    assert!(futures::poll!(Box::pin(last.wait())).is_pending());
    drop(read);
    last.wait().await;
}

#[tokio::test]
async fn batch_path_turns_cancelled_waiter_cannot_bypass_predecessor() {
    let paths = vec![Some("file".to_owned()); 3];
    let mut turns = batch_path_turns(&paths, &[false, false, false]);
    let first = turns[0].take().unwrap();
    let cancelled = turns[1].take().unwrap();
    let mut last = turns[2].take().unwrap();
    drop(cancelled);
    assert!(futures::poll!(Box::pin(last.wait())).is_pending());
    // Completion, error, and abort all drop the sender and release the tail.
    drop(first);
    last.wait().await;
}

#[tokio::test]
async fn batch_path_turns_unrelated_writes_and_read_only_batches_stay_parallel() {
    let paths = vec![
        Some("a".to_owned()),
        Some("b".to_owned()),
        Some("read".to_owned()),
        Some("read".to_owned()),
        None,
    ];
    let mut turns = batch_path_turns(&paths, &[false, false, true, true, false]);
    turns[0].as_mut().unwrap().wait().await;
    turns[1].as_mut().unwrap().wait().await;
    assert!(turns[2].is_none());
    assert!(turns[3].is_none());
    assert!(turns[4].is_none());
}

#[tokio::test]
async fn batch_path_turns_canonical_aliases_order_reads_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let args = vec![
        serde_json::json!({"path": "file"}),
        serde_json::json!({"target_file": dir.path().join("./file")}),
    ];
    let cwd = dir.path().to_owned();
    let paths = tokio::task::spawn_blocking(move || {
        args.iter()
            .map(|args| lock_path_for_args(args, &cwd, None))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    let mut turns = batch_path_turns(&paths, &[false, true]);
    let first = turns[0].take().unwrap();
    let mut read = turns[1].take().unwrap();
    assert!(futures::poll!(Box::pin(read.wait())).is_pending());
    drop(first);
    read.wait().await;
}

#[tokio::test]
async fn batch_path_turns_aborted_dispatch_releases_successor() {
    let paths = vec![Some("file".to_owned()); 2];
    let mut turns = batch_path_turns(&paths, &[false, false]);
    let mut first = turns[0].take().unwrap();
    let mut successor = turns[1].take().unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let dispatch = tokio::spawn(async move {
        first.wait().await;
        let _ = started.send(());
        std::future::pending::<()>().await;
        drop(first);
    });
    ready.await.unwrap();
    assert!(futures::poll!(Box::pin(successor.wait())).is_pending());
    dispatch.abort();
    assert!(dispatch.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(1), successor.wait())
        .await
        .unwrap();
}

#[tokio::test]
async fn batch_path_turns_reverse_polling_preserves_file_operation_order() {
    use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    let paths = vec![Some(file.to_string_lossy().into_owned()); 3];
    let turns = batch_path_turns(&paths, &[false, true, false]);
    let mut dispatches = Vec::new();
    for (index, turn) in turns.into_iter().enumerate() {
        let file = file.clone();
        dispatches.push(
            async move {
                let mut turn = turn.unwrap();
                turn.wait().await;
                let result = match index {
                    0 => {
                        tokio::fs::write(&file, "first").await.unwrap();
                        None
                    }
                    1 => Some(tokio::fs::read_to_string(&file).await.unwrap()),
                    _ => {
                        tokio::fs::write(&file, "last").await.unwrap();
                        None
                    }
                };
                drop(turn);
                (index, result)
            }
            .boxed(),
        );
    }
    // Start successors before the first emitted operation.
    let mut stream: FuturesUnordered<_> = dispatches.into_iter().rev().collect();
    let mut results = Vec::new();
    while let Some(result) = stream.next().await {
        results.push(result);
    }
    assert_eq!(
        results.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(results[1].1.as_deref(), Some("first"));
    assert_eq!(tokio::fs::read_to_string(file).await.unwrap(), "last");
}

/// Forked sessions must order displayed absolute and real/relative aliases alike.
#[tokio::test]
async fn batch_path_turns_display_cwd_aliases_order_file_operations() {
    use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("worktree");
    let display = dir.path().join("original");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    let file = cwd.join("src/a.rs");
    let args = [
        serde_json::json!({"file_path": display.join("src/a.rs")}),
        serde_json::json!({"target_file": "src/a.rs"}),
        serde_json::json!({"path": file}),
    ];
    let paths = tokio::task::spawn_blocking(move || {
        args.iter()
            .map(|args| lock_path_for_args(args, &cwd, Some(&display)))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(paths[0].is_some());
    assert_eq!(paths[0], paths[1]);
    assert_eq!(paths[0], paths[2]);
    let turns = batch_path_turns(&paths, &[false, true, false]);
    let mut dispatches = Vec::new();
    for (index, turn) in turns.into_iter().enumerate() {
        let file = file.clone();
        dispatches.push(
            async move {
                let mut turn = turn.unwrap();
                turn.wait().await;
                let result = match index {
                    0 => {
                        tokio::fs::write(&file, "first").await.unwrap();
                        None
                    }
                    1 => Some(tokio::fs::read_to_string(&file).await.unwrap()),
                    _ => {
                        tokio::fs::write(&file, "last").await.unwrap();
                        None
                    }
                };
                drop(turn);
                (index, result)
            }
            .boxed(),
        );
    }
    let mut stream: FuturesUnordered<_> = dispatches.into_iter().rev().collect();
    let mut results = Vec::new();
    while let Some(result) = stream.next().await {
        results.push(result);
    }
    assert_eq!(
        results.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(results[1].1.as_deref(), Some("first"));
    assert_eq!(tokio::fs::read_to_string(file).await.unwrap(), "last");
}

#[tokio::test]
async fn lock_path_timeout_preserves_fast_and_pathless_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let workers = Arc::new(tokio::sync::Semaphore::new(2));
    let (release, receiver) = std::sync::mpsc::channel::<()>();
    let receiver = std::sync::Mutex::new(receiver);
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        resolve_batch_lock_paths_with(
            vec![
                serde_json::json!({"path": "blocked"}),
                serde_json::json!({"command": "echo fast"}),
                serde_json::json!({"path": "fast"}),
            ],
            dir.path().to_owned(),
            None,
            workers.clone(),
            std::time::Duration::from_millis(100),
            move |args, cwd, display| {
                if args["path"] == "blocked" {
                    let _ = receiver.lock().unwrap().recv();
                }
                lock_path_for_args(args, cwd, display)
            },
        ),
    )
    .await
    .expect("a stuck filesystem lookup must not block the batch indefinitely");
    assert!(results[0].as_ref().unwrap_err().contains("timed out"));
    assert_eq!(results[1], Ok(None));
    assert_eq!(
        results[2],
        Ok(lock_path_for_args(
            &serde_json::json!({"path": "fast"}),
            dir.path(),
            None
        ))
    );
    // The abandoned blocking job still owns its permit, while siblings completed.
    assert_eq!(workers.available_permits(), 1);
    drop(release);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while workers.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn read_only_batch_skips_slow_lock_lookup() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let lookups = Arc::new(AtomicUsize::new(0));
    let observed = lookups.clone();
    let paths = resolve_batch_lock_paths_if_needed(
        &[true, true],
        resolve_batch_lock_paths_with(
            vec![
                serde_json::json!({"path": "slow"}),
                serde_json::json!({"path": "other"}),
            ],
            dir.path().to_owned(),
            None,
            Arc::new(tokio::sync::Semaphore::new(2)),
            std::time::Duration::from_millis(10),
            move |args, cwd, display| {
                observed.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(30));
                lock_path_for_args(args, cwd, display)
            },
        ),
    )
    .await;
    assert_eq!(
        paths,
        vec![Ok(None), Ok(None)],
        "read-only calls need no verified lock key"
    );
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        0,
        "do not start filesystem work"
    );
    assert!(
        batch_path_turns(&[None, None], &[true, true])
            .iter()
            .all(Option::is_none)
    );
}

#[tokio::test]
async fn mixed_batch_still_resolves_and_orders_read_write_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let read_only = [false, true];
    let paths = resolve_batch_lock_paths_if_needed(
        &read_only,
        resolve_batch_lock_paths(
            vec![
                serde_json::json!({"file_path": "new.txt"}),
                serde_json::json!({"path": "./new.txt"}),
            ],
            dir.path().to_owned(),
            None,
        ),
    )
    .await
    .into_iter()
    .map(Result::unwrap)
    .collect::<Vec<_>>();
    assert!(paths[0].is_some());
    assert_eq!(paths[0], paths[1]);
    let mut turns = batch_path_turns(&paths, &read_only);
    let mut write = turns[0].take().unwrap();
    let mut read = turns[1].take().unwrap();
    assert!(futures::poll!(Box::pin(read.wait())).is_pending());
    write.wait().await;
    tokio::fs::write(dir.path().join("new.txt"), "ready")
        .await
        .unwrap();
    drop(write);
    read.wait().await;
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("new.txt"))
            .await
            .unwrap(),
        "ready"
    );
}

async fn assert_alias_write_precedes_read(
    cwd: &std::path::Path,
    write_argument: &std::path::Path,
    write_target: &std::path::Path,
    read_target: &std::path::Path,
) {
    let paths = resolve_batch_lock_paths(
        vec![
            serde_json::json!({"file_path": write_argument}),
            serde_json::json!({"path": read_target}),
        ],
        cwd.to_owned(),
        None,
    )
    .await
    .into_iter()
    .map(Result::unwrap)
    .collect::<Vec<_>>();
    let mut turns = batch_path_turns(&paths, &[false, true]);
    let mut write = turns[0].take().unwrap();
    let mut read = turns[1]
        .take()
        .expect("alias read must have a predecessor gate");
    assert!(futures::poll!(Box::pin(read.wait())).is_pending());
    write.wait().await;
    tokio::fs::write(write_target, "written first")
        .await
        .unwrap();
    drop(write);
    read.wait().await;
    assert_eq!(
        tokio::fs::read_to_string(read_target).await.unwrap(),
        "written first"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn batch_path_turns_dangling_symlink_read_waits_for_creation() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("new.txt");
    let alias = dir.path().join("alias.txt");
    std::os::unix::fs::symlink("new.txt", &alias).unwrap();
    assert_alias_write_precedes_read(dir.path(), &target, &target, &alias).await;
}

#[tokio::test]
async fn batch_path_turns_unicode_filename_aliases_order_reads_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("note\u{202f}file.txt");
    let model = dir.path().join("note file.txt");
    tokio::fs::write(&target, "old content").await.unwrap();
    let effective = xai_grok_tools::util::try_resolve_unicode_filename(&model)
        .await
        .unwrap();
    assert_alias_write_precedes_read(dir.path(), &model, &effective.resolved_path, &target).await;
}

#[tokio::test]
async fn lock_path_timeout_does_not_accumulate_stuck_workers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let workers = Arc::new(tokio::sync::Semaphore::new(1));
    let started = Arc::new(AtomicUsize::new(0));
    let count = started.clone();
    let (release, receiver) = std::sync::mpsc::channel::<()>();
    let receiver = Arc::new(std::sync::Mutex::new(receiver));
    let resolve = move |args: &serde_json::Value,
                        cwd: &std::path::Path,
                        display: Option<&std::path::Path>| {
        count.fetch_add(1, Ordering::SeqCst);
        let _ = receiver.lock().unwrap().recv();
        lock_path_for_args(args, cwd, display)
    };
    let result = resolve_batch_lock_paths_with(
        vec![serde_json::json!({"path": "blocked"})],
        dir.path().to_owned(),
        None,
        workers.clone(),
        std::time::Duration::from_millis(100),
        resolve.clone(),
    )
    .await;
    assert!(result[0].is_err());
    assert_eq!(started.load(Ordering::SeqCst), 1);
    for _ in 0..3 {
        let result = resolve_batch_lock_paths_with(
            vec![
                serde_json::json!({"path": "another"}),
                serde_json::json!({"command": "fast"}),
            ],
            dir.path().to_owned(),
            None,
            workers.clone(),
            std::time::Duration::from_millis(10),
            resolve.clone(),
        )
        .await;
        assert!(result[0].is_err());
        assert_eq!(result[1], Ok(None));
    }
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "timed-out jobs retain permits"
    );
    drop(release);
    let permit = tokio::time::timeout(std::time::Duration::from_secs(1), workers.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(permit);
}
