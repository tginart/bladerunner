//! Purpose: Verify input encoding, grid isolation, resize, and PTY child cleanup.

use super::*;
use crossterm::event::KeyEvent;

#[test]
fn editor_keys_and_paste_follow_the_child_modes() {
    let up = Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(encode_input(&up, TermMode::empty()).unwrap(), b"\x1b[A");
    assert_eq!(encode_input(&up, TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
    let control_x = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(encode_input(&control_x, TermMode::empty()).unwrap(), [24]);
    let paste = Event::Paste("hello\nworld\x1b[201~".into());
    let bytes = encode_input(&paste, TermMode::BRACKETED_PASTE).unwrap();
    assert_eq!(bytes, b"\x1b[200~hello\rworld[201~\x1b[201~");
}

#[cfg(unix)]
#[test]
fn child_output_renders_and_resize_reaches_the_actual_pty() {
    use std::ffi::OsStr;
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let script = r"printf '\033[?1049h\033[2J\033[Hready'; read answer; stty size > size.txt";
    let mut pane = TerminalPane::spawn(
        "/bin/sh",
        &[OsStr::new("-c"), OsStr::new(script)],
        root.path(),
        30,
        8,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pane.poll();
        if pane.terminal.grid()[Line(0)][Column(0)].c == 'r' {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child did not paint its terminal"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let area = Rect::new(0, 0, 30, 8);
    let mut buf = Buffer::empty(area);
    pane.render(&mut buf, area, true);
    assert_eq!(buf[(0, 0)].symbol(), "r");
    pane.resize(42, 12).unwrap();
    pane.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
    while pane.is_running() {
        pane.poll();
        assert!(Instant::now() < deadline, "child did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(pane.exit_code, Some(0));
    assert_eq!(
        std::fs::read_to_string(root.path().join("size.txt"))
            .unwrap()
            .trim(),
        "12 42"
    );
}

#[cfg(unix)]
#[test]
fn drop_stops_the_editor_process_group() {
    use std::ffi::OsStr;
    let root = tempfile::tempdir().unwrap();
    let pane = TerminalPane::spawn(
        "/bin/sh",
        &[OsStr::new("-c"), OsStr::new("exec sleep 60")],
        root.path(),
        20,
        5,
    )
    .unwrap();
    let group = pane.group.as_ref().unwrap().clone();
    drop(pane);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while group.has_live_members() == Some(true) {
        assert!(
            std::time::Instant::now() < deadline,
            "editor process group survived pane drop"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "requires Neovim on PATH"]
fn neovim_edits_a_file_and_exits_inside_the_pane() {
    use std::ffi::OsStr;
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("with spaces.txt");
    std::fs::write(&path, "original\n").unwrap();
    let mut pane = TerminalPane::spawn(
        "nvim",
        &[
            OsStr::new("--clean"),
            OsStr::new("-n"),
            OsStr::new("-i"),
            OsStr::new("NONE"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
        root.path(),
        80,
        20,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pane.poll();
        if pane.terminal.mode().contains(TermMode::ALT_SCREEN)
            && pane.terminal.grid()[Line(0)][Column(0)].c == 'o'
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Neovim did not open the file: modes={:?}, status={:?}, error={:?}\n{}",
            pane.terminal.mode(),
            pane.exit_code,
            pane.error,
            screen_text(&pane)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pane.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Char('i'),
        KeyModifiers::NONE,
    )))
    .unwrap();
    pane.handle_input(&Event::Paste("added ".into())).unwrap();
    loop {
        pane.poll();
        if screen_text(&pane).contains("added original") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Neovim did not insert text: {:?}\n{}",
            pane.terminal.mode(),
            screen_text(&pane)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    for event in [
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    ] {
        pane.handle_input(&event).unwrap();
    }
    while pane.is_running() {
        pane.poll();
        assert!(Instant::now() < deadline, "Neovim did not save and exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pane.exit_code, Some(0));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "added original\n");
}

fn screen_text(pane: &TerminalPane) -> String {
    let mut text = String::new();
    for y in 0..pane.terminal.screen_lines() {
        for x in 0..pane.terminal.columns() {
            text.push(pane.terminal.grid()[Line(y as i32)][Column(x)].c);
        }
        text.push('\n');
    }
    text
}

#[cfg(unix)]
#[test]
#[ignore = "requires nano or Pico on PATH"]
fn nano_edits_a_file_and_exits_inside_the_pane() {
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("nano file.txt");
    std::fs::write(&path, "original\n").unwrap();
    let mut pane = TerminalPane::spawn("nano", &[path.as_os_str()], root.path(), 80, 24).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pane.poll();
        if screen_text(&pane).contains("original") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "nano did not open the file\n{}",
            screen_text(&pane)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pane.handle_input(&Event::Paste("added ".into())).unwrap();
    loop {
        pane.poll();
        if screen_text(&pane).contains("added original") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "nano did not insert text\n{}",
            screen_text(&pane)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pane.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    pane.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .unwrap();
    loop {
        pane.poll();
        if std::fs::read_to_string(&path).unwrap() == "added original\n" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "nano did not save the file\n{}",
            screen_text(&pane)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pane.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    while pane.is_running() {
        pane.poll();
        assert!(
            Instant::now() < deadline,
            "nano did not exit\n{}",
            screen_text(&pane)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pane.exit_code, Some(0));
}
