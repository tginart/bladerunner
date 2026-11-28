// Per-test-case module for the `pty_e2e` integration test crate.
//
// Regression pins for "the wheel scrolls the terminal, not the TUI". Apple
// Terminal reports the wheel to the app whenever mouse tracking is on; with
// tracking off it scrolls its own view into the scrollback above the alt
// screen. These tests check the terminal's final mode state from the raw
// byte stream: the last set/reset of a DEC mode decides whether it is on.
#[allow(unused_imports)]
use super::common::*;

const MOUSE_ON: &[u8] = b"\x1b[?1000h";
const MOUSE_OFF: &[u8] = b"\x1b[?1000l";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
const ALT_ON: &[u8] = b"\x1b[?1049h";
const ALT_OFF: &[u8] = b"\x1b[?1049l";

/// A `$PAGER` child (here via `/transcript`) that turns off mouse tracking and
/// bracketed paste at exit, as vim with `mouse=a` and less with `--mouse` do,
/// must not leave the pager without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn mouse_tracking_restored_after_pager_child() {
    let content = ContentController::start().await.expect("start content");
    let dir = tempfile::tempdir().expect("temp pager dir");
    let marker = dir.path().join("pager-ran");
    let script = dir.path().join("mode-resetting-pager.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf '\\033[?1000l\\033[?1002l\\033[?1003l\\033[?1006l\\033[?2004l'\n\
             touch '{}'\n",
            marker.display()
        ),
    )
    .expect("write pager script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("make pager executable");
    }

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_ops(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &[EnvOp::set("PAGER", &script.display().to_string())],
    )
    .expect("spawn pager");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    assert_mode_on(&harness, MOUSE_ON, MOUSE_OFF, "mouse tracking at startup");

    // `/transcript` needs a non-empty transcript; the sent prompt is enough.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness.update(Duration::from_secs(2));
    inject_keys_paced(&mut harness, b"/transcript");
    harness.inject_keys(b"\r").expect("run /transcript");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !marker.exists() {
        harness.update(Duration::from_millis(100));
    }
    assert!(
        marker.exists(),
        "precondition: $PAGER ran\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.update(Duration::from_secs(1));

    assert!(
        find_last(harness.raw_output(), b"\x1b[?1003l").is_some(),
        "precondition: the child's reset reached the terminal"
    );
    assert_mode_on(
        &harness,
        MOUSE_ON,
        MOUSE_OFF,
        "mouse tracking after the child",
    );
    assert_mode_on(
        &harness,
        PASTE_ON,
        PASTE_OFF,
        "bracketed paste after the child",
    );
    assert_mode_on(&harness, ALT_ON, ALT_OFF, "alt screen after the child");
    harness.quit().expect("clean quit");
}

/// Something outside the pager can turn mouse tracking off with no child
/// handoff and no focus report (Apple Terminal sends none). The pager must
/// send the mode again with a later frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn mouse_tracking_resent_with_later_frame() {
    let content = ContentController::start().await.expect("start content");
    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_ops(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &[],
    )
    .expect("spawn pager");
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness.update(Duration::from_secs(1));
    let startup_on = find_last(harness.raw_output(), MOUSE_ON);
    assert!(startup_on.is_some(), "mouse tracking at startup");

    // Longer than the pager's re-send interval; the key then causes a frame.
    harness.update(Duration::from_millis(2500));
    harness.inject_keys(b"x").expect("type a key");
    harness.update(Duration::from_secs(1));

    let later_on = find_last(harness.raw_output(), MOUSE_ON);
    assert!(
        later_on > startup_on,
        "mouse tracking must be sent again with a later frame \
         (startup set at {startup_on:?}, last set at {later_on:?})"
    );
    assert_mode_on(
        &harness,
        MOUSE_ON,
        MOUSE_OFF,
        "mouse tracking after re-send",
    );
    // Empty the prompt again so that `q` quits.
    harness.inject_keys(b"\x7f").expect("erase the key");
    harness.update(Duration::from_millis(200));
    harness.quit().expect("clean quit");
}

fn assert_mode_on(harness: &PtyHarness, on: &[u8], off: &[u8], what: &str) {
    let raw = harness.raw_output();
    let last_on = find_last(raw, on);
    let last_off = find_last(raw, off);
    assert!(
        last_on.is_some() && last_on > last_off,
        "{what}: must be on (last set at {last_on:?}, last reset at {last_off:?})\nscreen:\n{}",
        harness.screen_contents()
    );
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}
