//! Purpose: Verify lazy tree reads, preview ownership, and terminal-size changes.

use std::time::{Duration, Instant};

use super::*;

fn wait_for_reads(browser: &mut FileBrowserState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !browser.pending_directories.is_empty() || browser.preview_pending {
        browser.poll();
        assert!(Instant::now() < deadline, "browser read timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

#[test]
fn nested_directories_expand_lazily_and_close_without_a_file_write() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let file = nested.join("hello.rs");
    let content = "fn main() { println!(\"hello\"); }\n";
    std::fs::write(&file, content).unwrap();
    let mut browser = FileBrowserState::new(root.path()).unwrap();
    wait_for_reads(&mut browser);
    assert_eq!(browser.rows.len(), 2);
    assert!(!browser.directories.contains_key(&nested));
    browser.select(1);
    browser.handle_key(&key(KeyCode::Enter));
    wait_for_reads(&mut browser);
    assert_eq!(browser.rows.len(), 3);
    browser.select(2);
    browser.handle_key(&key(KeyCode::Enter));
    wait_for_reads(&mut browser);
    assert_eq!(browser.preview_path, Some(file.clone()));
    assert!(browser.preview.is_some());
    assert!(
        matches!(browser.handle_key(&key(KeyCode::Char('o'))), BrowserAction::OpenDefault(path) if path == file)
    );
    assert!(matches!(
        browser.handle_key(&key(KeyCode::Esc)),
        BrowserAction::Close
    ));
    assert_eq!(std::fs::read_to_string(file).unwrap(), content);
}

#[test]
fn stale_preview_results_do_not_replace_the_current_file() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first.txt");
    let last = root.path().join("last.txt");
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&last, "last").unwrap();
    let mut browser = FileBrowserState::new(root.path()).unwrap();
    browser.open_preview(&first);
    browser.open_preview(&last);
    wait_for_reads(&mut browser);
    assert_eq!(browser.preview.as_ref().unwrap().path, last);
}

#[test]
fn small_frames_and_resize_clear_invisible_mouse_targets() {
    let root = tempfile::tempdir().unwrap();
    let mut browser = FileBrowserState::new(root.path()).unwrap();
    for (width, height) in [(100, 24), (40, 15), (8, 5), (1, 1), (0, 0)] {
        let area = Rect::new(3, 2, width, height);
        let mut buf = Buffer::empty(area);
        browser.render(&mut buf, area);
        for hit in [
            browser.hits.tree,
            browser.hits.preview,
            browser.hits.open,
            browser.hits.close,
        ] {
            if hit.area() > 0 {
                assert_eq!(hit.intersection(area), hit);
            }
        }
    }
    assert_eq!(browser.hits.open, Rect::default());
}

#[test]
fn preview_errors_leave_native_open_available() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("binary.dat");
    std::fs::write(&path, [0, 1]).unwrap();
    let mut browser = FileBrowserState::new(root.path()).unwrap();
    browser.open_preview(&path);
    browser.focus = Focus::Preview;
    wait_for_reads(&mut browser);
    assert!(browser.preview_notice.as_ref().unwrap().contains("Binary"));
    assert!(
        matches!(browser.handle_key(&key(KeyCode::Char('o'))), BrowserAction::OpenDefault(file) if file == path)
    );
}

#[test]
fn editor_choice_uses_the_selected_file_even_when_another_preview_is_open() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&second, "second").unwrap();
    let mut browser = FileBrowserState::new(root.path()).unwrap();
    browser.open_preview(&first);
    wait_for_reads(&mut browser);
    browser.focus = Focus::Tree;
    let index = browser
        .rows
        .iter()
        .position(|row| row.entry.path == second)
        .unwrap();
    browser.select(index);
    browser.handle_key(&key(KeyCode::Char('e')));
    let action = browser.handle_key(&key(KeyCode::Enter));
    assert!(
        matches!(action, BrowserAction::Edit { path, editor: Editor::Neovim } if path == second)
    );
}
