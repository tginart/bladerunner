//! Real OS signals in isolated child processes, including the complete headless/agent lifecycle.
//! Children run this libtest fixture, so no installed binary or real account is required.
use super::*;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use xai_grok_pager_pty_harness::ContentController;
use xai_grok_test_support::{
    ScriptedResponse, TestOutput, TestProcess, TestProcessConfig, TestProcessTree, TestSandbox,
};

const FIXTURE: &str = "headless::signal_lifecycle_tests::signal_subprocess_fixture";
const MODE_ENV: &str = "BLADERUNNER_SIGNAL_TEST_MODE";

fn options() -> HeadlessOptions {
    HeadlessOptions {
        session_id: None,
        resume: None,
        resume_title_pinned: false,
        cwd: None,
        yolo: true,
        trust: true,
        output_format: OutputFormat::Plain,
        include_partial_messages: false,
        json_schema: None,
        model: Some("test-model".into()),
        rules: None,
        system_prompt_override: None,
        continue_last_session: false,
        fork_session: false,
        worktree: None,
        restore_code: false,
        hub_startup_command: None,
        agent: None,
        agents_json: None,
        cli_tools: None,
        cli_disallowed_tools: None,
        disable_web_search: true,
        allow_rules: vec![],
        deny_rules: vec![],
        max_turns: None,
        permission_mode_flag: None,
        reasoning_effort: None,
        wait_for_background: true,
        background_wait_timeout: Duration::from_secs(60),
    }
}

#[test]
fn signal_subprocess_fixture() {
    let Ok(mode) = std::env::var(MODE_ENV) else {
        return;
    };
    std::thread::Builder::new()
        .name("headless-signal-fixture".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                if mode == "run" {
                    run_single_turn(
                        HeadlessPrompt::Text("signal regression".into()),
                        false,
                        options(),
                    )
                    .await
                    .expect("headless run");
                    panic!("the pending inference turn must only end on a signal");
                }
                let signals = HeadlessSignals::install()
                    .await
                    .expect("install before agent startup");
                let deferred = (mode == "stalled-cleanup").then(|| signals.defer_exit());
                println!("SIGNAL_WATCHER_READY");
                std::io::stdout().flush().unwrap();
                if let Some(deferred) = deferred {
                    deferred.signalled().await;
                    println!("FIRST_SIGNAL_CLEANUP_STARTED");
                    std::io::stdout().flush().unwrap();
                }
                // Deliberately block the only async worker; the independent watcher must exit.
                std::thread::sleep(Duration::from_secs(120));
                panic!("signal watcher did not exit");
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn spawn_fixture(sandbox: &TestSandbox, mode: &str) -> TestProcess {
    if mode == "run" {
        // Production defaults allow only Codex/AIP. Permit the isolated loopback model.
        std::fs::write(
            sandbox.grok_home().join("config.toml"),
            r#"[models]
default = "test-model"
allowed_models = ["test-model"]
web_search = "test-model"
session_summary = "test-model"
image_description = "test-model"
prompt_suggestion = "test-model"
[subagents]
enabled = false
"#,
        )
        .unwrap();
    }
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", FIXTURE, "--nocapture"]);
    command.current_dir(sandbox.workspace());
    TestProcess::spawn(
        command,
        sandbox,
        TestProcessConfig::new()
            .label("headless signal fixture")
            .stdout(TestOutput::Capture)
            .stderr(TestOutput::Capture)
            .env(MODE_ENV, mode),
    )
    .unwrap()
}

async fn wait_mock_barrier(barrier: impl Future<Output = ()>, process: &mut TestProcess) {
    tokio::pin!(barrier);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        tokio::select! {
            () = &mut barrier => return,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                assert!(process.try_wait().unwrap().is_none(), "child exited before mock barrier: {}", process.diagnostic_summary());
                assert!(tokio::time::Instant::now() < deadline, "mock barrier timed out: {}", process.diagnostic_summary());
            }
        }
    }
}

async fn wait_text(process: &mut TestProcess, text: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if process.stdout_tail().text.contains(text) {
            return;
        }
        assert!(
            process.try_wait().unwrap().is_none(),
            "child exited before {text}: {}",
            process.diagnostic_summary()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "missing {text}: {}",
            process.diagnostic_summary()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn send_signal(process: &TestProcess, signal: libc::c_int) {
    // The PID belongs to this test's isolated child, not the test runner.
    assert_eq!(0, unsafe {
        libc::kill(process.pid().unwrap() as libc::pid_t, signal)
    });
}

async fn assert_exit(process: &mut TestProcess, code: i32) {
    let status = process
        .wait_with_deadline(Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("signal exit timed out: {}", process.diagnostic_summary()));
    assert_eq!(
        status.code(),
        Some(code),
        "{}",
        process.diagnostic_summary()
    );
}

#[tokio::test]
async fn startup_watcher_claims_all_signals_before_an_agent_exists() {
    for (signal, code) in [
        (libc::SIGINT, 130),
        (libc::SIGTERM, 143),
        (libc::SIGHUP, 129),
    ] {
        let sandbox = TestSandbox::new();
        let mut process = spawn_fixture(&sandbox, "startup");
        wait_text(&mut process, "SIGNAL_WATCHER_READY").await;
        send_signal(&process, signal);
        assert_exit(&mut process, code).await;
    }
}

#[tokio::test]
async fn second_os_signal_exits_during_stalled_cleanup() {
    let sandbox = TestSandbox::new();
    let mut process = spawn_fixture(&sandbox, "stalled-cleanup");
    wait_text(&mut process, "SIGNAL_WATCHER_READY").await;
    send_signal(&process, libc::SIGTERM);
    wait_text(&mut process, "FIRST_SIGNAL_CLEANUP_STARTED").await;
    assert!(
        process.try_wait().unwrap().is_none(),
        "first signal allows cleanup"
    );
    send_signal(&process, libc::SIGHUP);
    assert_exit(&mut process, 129).await;
}

fn json_has_cancelled_turn(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            let is_turn =
                object.get("sessionUpdate").and_then(|v| v.as_str()) == Some("turn_completed");
            (is_turn
                && object
                    .get("stop_reason")
                    .or_else(|| object.get("stopReason"))
                    .and_then(|v| v.as_str())
                    == Some("cancelled"))
                || object.values().any(json_has_cancelled_turn)
        }
        serde_json::Value::Array(values) => values.iter().any(json_has_cancelled_turn),
        _ => false,
    }
}

fn persisted_text(root: &Path, text: &str) -> bool {
    std::fs::read_dir(root).unwrap().any(|entry| {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            persisted_text(&entry.path(), text)
        } else {
            entry.file_name() == "updates.jsonl"
                && std::fs::read_to_string(entry.path())
                    .unwrap()
                    .contains(text)
        }
    })
}

fn persisted_cancelled_turn(root: &Path) -> bool {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            if persisted_cancelled_turn(&entry.path()) {
                return true;
            }
        } else if entry.file_name() == "updates.jsonl" {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            if content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .any(|v| json_has_cancelled_turn(&v))
            {
                return true;
            }
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_signal_persists_cancelled_turn_and_prior_streamed_text() {
    let content = ContentController::start().await.unwrap();
    let mut turn = content.expect_agent_turn_blocked("signal pending", "SIGNAL_PARTIAL_OUTPUT");
    let mut process = spawn_fixture(content.sandbox(), "run");
    wait_mock_barrier(turn.wait_blocked(), &mut process).await;
    wait_text(&mut process, "SIGNAL_PARTIAL_OUTPUT").await;
    send_signal(&process, libc::SIGINT);
    assert_exit(&mut process, 130).await;
    assert!(
        persisted_cancelled_turn(content.sandbox().grok_home()),
        "unfinished turn must have durable Cancelled outcome: {}",
        process.diagnostic_summary()
    );
    assert!(
        persisted_text(content.sandbox().grok_home(), "SIGNAL_PARTIAL_OUTPUT"),
        "prior streamed text must remain durable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_takes_priority_over_a_busy_stream() {
    let content = ContentController::start().await.unwrap();
    let text = format!("SIGNAL_FIREHOSE {}", "stream ".repeat(10_000));
    let mut turn = content.expect_agent_turn_blocked("signal busy stream", &text);
    let mut process = spawn_fixture(content.sandbox(), "run");
    wait_mock_barrier(turn.wait_received(), &mut process).await;
    wait_text(&mut process, "stream ").await;
    send_signal(&process, libc::SIGTERM);
    assert_exit(&mut process, 143).await;
    assert!(persisted_cancelled_turn(content.sandbox().grok_home()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_signal_reaps_detached_background_process() {
    let content = ContentController::start().await.unwrap();
    let pidfile = content.sandbox().workspace().join("signal-background.pid");
    let arguments = serde_json::json!({
        "command": format!("echo $$ > '{}'; /bin/sleep 600", pidfile.display()),
        "description": "signal background cleanup regression",
        "is_background": true,
    })
    .to_string();
    let _tool_turn = content.expect_agent_turn_with_responses(
        "start background",
        ScriptedResponse::sse(
            xai_grok_test_support::sse::responses_api_reasoning_then_tool_call_events(
                "",
                "signal_bg",
                "run_terminal_command",
                &arguments,
                "test-model",
            ),
        ),
        ScriptedResponse::sse(
            xai_grok_test_support::sse::chat_completions_reasoning_then_tool_call_events(
                "",
                "signal_bg",
                "run_terminal_command",
                &arguments,
                "test-model",
            ),
        ),
    );
    let mut turn =
        content.expect_agent_turn_blocked("signal after background", "SIGNAL_BACKGROUND_RUNNING");
    let mut process = spawn_fixture(content.sandbox(), "run");
    wait_mock_barrier(turn.wait_blocked(), &mut process).await;
    wait_text(&mut process, "SIGNAL_BACKGROUND_RUNNING").await;
    let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 1);
    // Preserve failure cleanup even when the detached command outlives its parent.
    let mut background_guard = TestProcessTree::attach(pid as u32, "signal background fixture");
    assert_eq!(
        0,
        unsafe { libc::kill(pid, 0) },
        "background is alive before signal"
    );
    send_signal(&process, libc::SIGINT);
    assert_exit(&mut process, 130).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background command must not survive signal exit");
    background_guard.release();
    assert!(persisted_cancelled_turn(content.sandbox().grok_home()));
}
