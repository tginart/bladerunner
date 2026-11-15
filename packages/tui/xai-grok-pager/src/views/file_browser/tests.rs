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

/*
Purpose: Keep Vim command keys out of the host Escape shortcut.
Input: Escape and d events while the child terminal has focus.
Output: The child receives both bytes and the host requests no debug action.
*/
#[cfg(unix)]
#[test]
fn focused_editor_receives_escape_then_d_without_the_host_debug_action() {
    use crate::actions::ActionRegistry;
    use crate::app::app_view::InputOutcome;

    let root = tempfile::tempdir().unwrap();
    let mut agent = crate::test_util::make_agent_view(None, root.path().to_str().unwrap());
    agent.file_browser = Some(browser_with_terminal(
        root.path(),
        "stty raw -echo; printf ready > ready; dd bs=1 count=2 of=keys 2>/dev/null",
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.path().join("ready").exists() {
        assert!(Instant::now() < deadline, "child did not enter raw mode");
        std::thread::sleep(Duration::from_millis(5));
    }
    let registry = ActionRegistry::defaults();
    for code in [KeyCode::Esc, KeyCode::Char('d')] {
        assert!(matches!(
            agent.handle_input(&Event::Key(key(code)), &registry),
            InputOutcome::Changed
        ));
    }
    while agent.file_browser.as_ref().unwrap().has_editor() {
        agent.poll_file_browser();
        assert!(Instant::now() < deadline, "child did not receive both keys");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(std::fs::read(root.path().join("keys")).unwrap(), b"\x1bd");
}

/*
Purpose: Verify that a hidden session cannot block its editor output queue.
Input: Inactive root sessions and child sessions on each host screen.
Output: Each child writes more than the queue capacity and exits without a screen switch.
*/
#[cfg(unix)]
#[test]
fn inactive_editors_drain_output_and_exit_on_every_host_screen() {
    use crate::app::agent::AgentId;
    use crate::app::app_view::{ActiveView, TickDemand, tests::test_app};

    for active_view in [
        ActiveView::Welcome,
        ActiveView::AgentDashboard,
        ActiveView::Agent(AgentId(1)),
    ] {
        for is_child in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().to_str().unwrap();
            let mut owner = crate::test_util::make_agent_view(None, cwd);
            owner.file_browser = Some(browser_with_terminal(
                root.path(),
                "dd if=/dev/zero bs=4096 count=128 2>/dev/null; printf complete > done",
            ));
            let mut app = test_app();
            if is_child {
                let mut parent = crate::test_util::make_agent_view(None, cwd);
                parent
                    .subagent_views
                    .insert("child".into(), Box::new(owner));
                app.agents.insert(AgentId(0), parent);
            } else {
                app.agents.insert(AgentId(0), owner);
            }
            let mut other = crate::test_util::make_agent_view(None, cwd);
            other.session.id = AgentId(1);
            app.agents.insert(AgentId(1), other);
            app.active_view = active_view;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert_eq!(app.tick_demand(), TickDemand::Fast);
                app.tick();
                let parent = app.agents.get(&AgentId(0)).unwrap();
                let owner = if is_child {
                    parent.subagent_views.get("child").unwrap().as_ref()
                } else {
                    parent
                };
                if !owner.file_browser.as_ref().unwrap().has_editor() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "inactive editor stalled on {active_view:?}, is_child={is_child}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                std::fs::read_to_string(root.path().join("done")).unwrap(),
                "complete"
            );
        }
    }
}

/*
Purpose: Attach a real PTY child to the browser without an external editor dependency.
Input: The test directory and the shell program.
Output: A browser whose preview gives input to the PTY child.
*/
#[cfg(unix)]
fn browser_with_terminal(root: &Path, script: &str) -> FileBrowserState {
    use std::ffi::OsStr;

    let path = root.join("source.txt");
    std::fs::write(&path, "source\n").unwrap();
    let mut browser = FileBrowserState::new(root).unwrap();
    wait_for_reads(&mut browser);
    browser.editor = Some(EditorPane {
        terminal: TerminalPane::spawn(
            "/bin/sh",
            &[OsStr::new("-c"), OsStr::new(script)],
            root,
            80,
            20,
        )
        .unwrap(),
        path,
        editor: Editor::Neovim,
    });
    browser.focus = Focus::Preview;
    browser
}
