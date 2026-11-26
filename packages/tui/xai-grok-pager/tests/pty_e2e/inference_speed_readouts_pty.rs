// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// The line of `screen` that contains `needle`, if any.
fn line_with<'a>(screen: &'a str, needle: &str) -> Option<&'a str> {
    screen.lines().find(|line| line.contains(needle))
}

/// Turn the readout on and route the default model to the mock server. The
/// stock `palantir-aip` provider only talks to a Foundry hostname, so the
/// test model is a plain custom model with its own `base_url`.
fn seed_speed_config(content: &ContentController) {
    let grok_home = content.home().join(".grok");
    std::fs::create_dir_all(&grok_home).expect("create .grok");
    let config = format!(
        "[ui]\nlanguage = \"en\"\nshow_inference_speed = true\n\n\
         [models]\ndefault = \"mock\"\nallowed_models = [\"mock\"]\n\n\
         [model.mock]\nmodel = \"test-model\"\nbase_url = \"{}\"\napi_key = \"test\"\n",
        content.url()
    );
    std::fs::write(grok_home.join("config.toml"), config).expect("write config.toml");
}

/// With `[ui] show_inference_speed = true`, the tok/s readout shows in two
/// places: live after "Responding… Ns" while the model streams, and as the
/// turn average after "Worked for Ns". The retired top-right chip (and its
/// TTFT value) must not come back.
///
/// Drives the real binary: the mock streams one word per 100ms, so the live
/// moving average has several seconds of stream to measure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn inference_speed_shows_live_and_after_worked_for() {
    let content = ContentController::start().await.expect("start content");
    seed_speed_config(&content);
    let words = vec!["streaming"; 50].join(" ");
    content.set_response(format!("{words} {MOCK_RESPONSE_SENTINEL}"));
    content.set_chunk_delay(Some(Duration::from_millis(100)));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    // Live: the estimate sits on the turn status row, after the label.
    harness
        .wait_until(
            "live tok/s on the Responding row",
            Duration::from_secs(20),
            |h| {
                line_with(&h.screen_contents(), "Responding")
                    .is_some_and(|line| line.contains("~") && line.contains("tok/s"))
            },
        )
        .unwrap_or_else(|e| panic!("{e}\nscreen:\n{}", harness.screen_contents()));
    let live_screen = harness.screen_contents();
    let live_line = line_with(&live_screen, "Responding").expect("status row");
    let (label, speed) = live_line.split_once('~').expect("estimate marker");
    assert!(
        label.trim_end().ends_with('\u{00b7}'),
        "speed follows the label and phase timer: {live_line}"
    );
    assert!(
        label.find("Responding") < label.find('\u{00b7}'),
        "speed is to the right of the label: {live_line}"
    );
    assert!(
        speed.contains("tok/s"),
        "unit follows the value: {live_line}"
    );
    assert!(
        !live_screen.contains("TTFT"),
        "TTFT stays hidden\nscreen:\n{live_screen}"
    );

    // Final: the turn average sits after the "Worked for" duration.
    harness
        .wait_until(
            "tok/s on the Worked for marker",
            Duration::from_secs(30),
            |h| {
                line_with(&h.screen_contents(), "Worked for")
                    .is_some_and(|line| line.contains("tok/s"))
            },
        )
        .unwrap_or_else(|e| panic!("{e}\nscreen:\n{}", harness.screen_contents()));
    let final_screen = harness.screen_contents();
    let marker = line_with(&final_screen, "Worked for").expect("marker line");
    assert!(
        marker.find("Worked for") < marker.find("tok/s"),
        "speed is to the right of the duration: {marker}"
    );
    assert!(
        !marker.contains('~'),
        "the turn average is measured, not estimated: {marker}"
    );
    assert_eq!(
        final_screen.matches("tok/s").count(),
        1,
        "only the marker shows a speed once the turn ends\nscreen:\n{final_screen}"
    );
    assert!(
        !final_screen.contains("TTFT"),
        "TTFT stays hidden\nscreen:\n{final_screen}"
    );
    println!("LIVE ROW:   {}", live_line.trim());
    println!("FINAL ROW:  {}", marker.trim());

    harness.quit().expect("clean quit");
}
