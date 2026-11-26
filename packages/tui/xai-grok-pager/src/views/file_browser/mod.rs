/*
Purpose: Maintain a lazy file tree and a read-only source preview.
Input: Keyboard events, mouse events, and results from the file worker.
Output: View state and explicit requests for host actions.
*/

mod fs;
mod layout;
mod render;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::file_search::line_viewer::LineViewerState;
use super::terminal_pane::TerminalPane;
use crate::clipboard::SystemClipboard;
use fs::{Entry, Request, Response};
pub(crate) use layout::{BrowserLayout, split_area};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Tree,
    Preview,
}

pub(crate) enum BrowserAction {
    Changed,
    Close,
    FocusPrompt,
    OpenDefault(PathBuf),
    Edit { path: PathBuf, editor: Editor },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Editor {
    Neovim,
    Vim,
    Nano,
}

impl Editor {
    pub(crate) fn command(self) -> &'static str {
        match self {
            Self::Neovim => "nvim",
            Self::Vim => "vim",
            Self::Nano => "nano",
        }
    }
}

const EDITORS: [Editor; 3] = [Editor::Neovim, Editor::Vim, Editor::Nano];

struct EditorPane {
    terminal: TerminalPane,
    path: PathBuf,
    editor: Editor,
}

#[derive(Clone)]
struct TreeRow {
    entry: Entry,
    depth: usize,
}

#[derive(Default)]
struct HitAreas {
    area: Rect,
    tree: Rect,
    preview: Rect,
    open: Rect,
    edit: Rect,
    refresh: Rect,
    show_all: Rect,
    layout: Rect,
    close: Rect,
    editor_choices: [Rect; 3],
}

/*
Purpose: Keep browser state for one agent session.
Input: The session directory. File reads use a dedicated worker.
Output: A reusable tree and preview that do not alter the prompt draft.
*/
pub struct FileBrowserState {
    pub(crate) visible: bool,
    pub(crate) focused: bool,
    root: PathBuf,
    layout: BrowserLayout,
    focus: Focus,
    show_all: bool,
    expanded: HashSet<PathBuf>,
    directories: HashMap<PathBuf, Vec<Entry>>,
    pending_directories: HashSet<PathBuf>,
    rows: Vec<TreeRow>,
    selected: usize,
    tree_scroll: usize,
    preview_path: Option<PathBuf>,
    preview: Option<LineViewerState>,
    preview_notice: Option<String>,
    notice: Option<String>,
    epoch: u64,
    preview_generation: u64,
    preview_pending: bool,
    requests: Sender<Request>,
    responses: Receiver<Response>,
    hits: HitAreas,
    editor: Option<EditorPane>,
    editor_choice: Option<usize>,
    editor_target: Option<PathBuf>,
    preferred_editor: usize,
}

impl FileBrowserState {
    /*
    Purpose: Start one file worker and request the root directory.
    Input: The session directory.
    Output: An empty browser that can accept the first directory result.
    */
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        let (requests, request_rx) = mpsc::channel();
        let (response_tx, responses) = mpsc::channel();
        std::thread::Builder::new()
            .name("file-browser".into())
            .spawn(move || fs::run(request_rx, response_tx))?;
        let mut browser = Self {
            visible: true,
            focused: true,
            root: root.to_path_buf(),
            layout: BrowserLayout::default(),
            focus: Focus::Tree,
            show_all: false,
            expanded: HashSet::from([root.to_path_buf()]),
            directories: HashMap::new(),
            pending_directories: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            tree_scroll: 0,
            preview_path: None,
            preview: None,
            preview_notice: None,
            notice: None,
            epoch: 0,
            preview_generation: 0,
            preview_pending: false,
            requests,
            responses,
            hits: HitAreas::default(),
            editor: None,
            editor_choice: None,
            editor_target: None,
            preferred_editor: 0,
        };
        browser.rebuild_rows();
        browser.load_directory(root);
        Ok(browser)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn layout(&self) -> BrowserLayout {
        self.layout
    }

    fn cycle_layout(&mut self) {
        self.layout = self.layout.next();
        self.notice = Some(format!("Files layout: {}", self.layout.label()));
    }

    pub(crate) fn needs_tick(&self) -> bool {
        !self.pending_directories.is_empty()
            || self.preview_pending
            || self.editor.is_some()
            || self
                .preview
                .as_ref()
                .is_some_and(|preview| preview.list_state.needs_tick())
    }

    /*
    Purpose: Apply completed reads without an old request replacing a new selection.
    Input: The worker responses that are ready now.
    Output: True when the browser needs a new frame.
    */
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(editor) = &mut self.editor {
            changed |= editor.terminal.poll();
            if let Some(error) = editor.terminal.error.take() {
                self.notice = Some(format!("Editor terminal: {error}"));
                changed = true;
            }
            if !editor.terminal.is_running() {
                let path = editor.path.clone();
                let status = editor.terminal.exit_code.unwrap_or(1);
                self.notice = Some(format!("{} exited ({status})", editor.editor.command()));
                self.editor = None;
                self.open_preview(&path);
                changed = true;
            }
        }
        while let Ok(response) = self.responses.try_recv() {
            match response {
                Response::Directory {
                    path,
                    epoch,
                    result,
                } if epoch == self.epoch => {
                    self.pending_directories.remove(&path);
                    match result {
                        Ok(entries) => {
                            self.directories.insert(path, entries);
                        }
                        Err(error) => {
                            self.expanded.remove(&path);
                            self.notice = Some(format!("{}: {error}", path.display()));
                        }
                    }
                    self.rebuild_rows();
                    changed = true;
                }
                Response::Preview {
                    path,
                    generation,
                    result,
                } if generation == self.preview_generation
                    && self.preview_path.as_ref() == Some(&path) =>
                {
                    self.preview_pending = false;
                    match result {
                        Ok(data) => {
                            let mut preview =
                                LineViewerState::from_source_lines(&path, data.lines, None);
                            preview
                                .list_state
                                .set_clipboard_provider(Box::new(SystemClipboard));
                            self.preview = Some(preview);
                            self.preview_notice = data.is_truncated.then(|| {
                                "Preview truncated; open the file for its full content".into()
                            });
                        }
                        Err(error) => self.preview_notice = Some(error),
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        if let Some(preview) = &mut self.preview {
            changed |= preview.list_state.tick();
        }
        changed
    }

    /*
    Purpose: Route browser keys without a write path to the file.
    Input: A key event while the tree or preview has focus.
    Output: A host action, or a request to redraw the browser.
    */
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> BrowserAction {
        if self.editor_choice.is_some() {
            return self.handle_editor_choice(key);
        }
        if self.terminal_has_focus() {
            if key.code == KeyCode::Char(']') && key.modifiers == KeyModifiers::CONTROL {
                self.focus = Focus::Tree;
            } else if let Some(editor) = &mut self.editor
                && let Err(error) = editor.terminal.handle_input(&Event::Key(*key))
            {
                self.notice = Some(error.to_string());
            }
            return BrowserAction::Changed;
        }
        if self.focus == Focus::Preview
            && let Some(preview) = &mut self.preview
            && preview.list_state.input_mode().is_some()
        {
            preview.list_state.handle_key_event(key, &preview.lines);
            return BrowserAction::Changed;
        }
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            if self.focus == Focus::Tree {
                self.focus = Focus::Preview;
                return BrowserAction::Changed;
            }
            return BrowserAction::FocusPrompt;
        }
        if key.code == KeyCode::BackTab {
            if self.focus == Focus::Preview {
                self.focus = Focus::Tree;
                return BrowserAction::Changed;
            }
            return BrowserAction::FocusPrompt;
        }
        if crate::key!('L').matches(key) {
            self.cycle_layout();
            return BrowserAction::Changed;
        }
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return BrowserAction::Close,
                KeyCode::Char('o') => {
                    if let Some(path) = self.action_path() {
                        return BrowserAction::OpenDefault(path);
                    }
                }
                KeyCode::Char('r') => self.refresh(),
                KeyCode::Char('e') => self.choose_editor(),
                KeyCode::Char('.') => {
                    self.show_all = !self.show_all;
                    self.refresh();
                }
                _ => {}
            }
        }
        match self.focus {
            Focus::Tree => self.handle_tree_key(key),
            Focus::Preview => {
                if let Some(preview) = &mut self.preview {
                    preview.list_state.handle_key_event(key, &preview.lines);
                }
            }
        }
        BrowserAction::Changed
    }

    /*
    Purpose: Keep mouse actions inside the rectangles from the last browser frame.
    Input: A mouse event and terminal coordinates.
    Output: The action for a button, tree entry, or preview.
    */
    pub(crate) fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
    ) -> BrowserAction {
        let point = (column, row).into();
        if kind == MouseEventKind::Down(MouseButton::Left) {
            self.focused = true;
            if self.hits.close.contains(point) {
                return BrowserAction::Close;
            }
            if self.hits.layout.contains(point) {
                self.cycle_layout();
                return BrowserAction::Changed;
            }
            if self.editor_choice.is_some() {
                if let Some(index) = self
                    .hits
                    .editor_choices
                    .iter()
                    .position(|area| area.contains(point))
                {
                    self.editor_choice = Some(index);
                    return self
                        .handle_editor_choice(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
                return BrowserAction::Changed;
            }
            if self.hits.open.contains(point)
                && let Some(path) = self.action_path()
            {
                return BrowserAction::OpenDefault(path);
            }
            if self.hits.edit.contains(point) {
                self.choose_editor();
            } else if self.hits.refresh.contains(point) {
                self.refresh();
            } else if self.hits.show_all.contains(point) {
                self.show_all = !self.show_all;
                self.refresh();
            } else if self.hits.tree.contains(point) {
                self.focus = Focus::Tree;
                let index = self.tree_scroll + (row - self.hits.tree.y) as usize;
                if index < self.rows.len() {
                    self.selected = index;
                    self.activate_selected();
                }
            } else if self.hits.preview.contains(point) {
                self.focus = Focus::Preview;
            }
        }
        if self.hits.preview.contains(point) {
            if let Some(editor) = &mut self.editor {
                let mouse = MouseEvent {
                    kind,
                    column: column - self.hits.preview.x,
                    row: row - self.hits.preview.y,
                    modifiers: KeyModifiers::NONE,
                };
                if let Err(error) = editor.terminal.handle_input(&Event::Mouse(mouse)) {
                    self.notice = Some(error.to_string());
                }
            } else if self.editor_choice.is_none()
                && let Some(preview) = &mut self.preview
            {
                preview.list_state.handle_mouse_event(
                    kind,
                    column,
                    row,
                    self.hits.preview,
                    &preview.lines,
                );
            }
        }
        BrowserAction::Changed
    }

    pub(crate) fn contains(&self, column: u16, row: u16) -> bool {
        self.hits.area.contains((column, row).into())
    }

    pub(crate) fn focus_tree(&mut self) {
        self.focused = true;
        self.focus = Focus::Tree;
        self.editor_choice = None;
        self.editor_target = None;
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if self.terminal_has_focus()
            && let Some(editor) = &mut self.editor
        {
            if let Err(error) = editor.terminal.handle_input(&Event::Paste(text.into())) {
                self.notice = Some(error.to_string());
            }
            return;
        }
        if self.focus == Focus::Preview
            && let Some(preview) = &mut self.preview
        {
            preview.list_state.handle_paste(text, &preview.lines);
        }
    }

    pub(crate) fn handle_scroll(&mut self, lines: i32, column: u16, row: u16) {
        if self.hits.tree.contains((column, row).into()) {
            let max_scroll = self
                .rows
                .len()
                .saturating_sub(self.hits.tree.height as usize);
            self.tree_scroll = self
                .tree_scroll
                .saturating_add_signed(lines as isize)
                .min(max_scroll);
        } else if self.hits.preview.contains((column, row).into()) {
            if let Some(editor) = &mut self.editor {
                let kind = if lines > 0 {
                    MouseEventKind::ScrollDown
                } else {
                    MouseEventKind::ScrollUp
                };
                let mouse = MouseEvent {
                    kind,
                    column: column - self.hits.preview.x,
                    row: row - self.hits.preview.y,
                    modifiers: KeyModifiers::NONE,
                };
                for _ in 0..lines.unsigned_abs().min(20) {
                    if let Err(error) = editor.terminal.handle_input(&Event::Mouse(mouse)) {
                        self.notice = Some(error.to_string());
                        break;
                    }
                }
            } else if let Some(preview) = &mut self.preview {
                preview.list_state.scroll_lines(lines, &preview.lines);
            }
        }
    }

    pub(crate) fn render(&mut self, buf: &mut Buffer, area: Rect) {
        render::render(self, buf, area);
    }

    pub(crate) fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /*
    Purpose: Describe the keys that the current browser surface consumes.
    Input: The current editor, chooser, and focus state.
    Output: Shortcut hints for the shared footer.
    */
    pub(crate) fn shortcut_hints(&self) -> Vec<crate::views::shortcuts_bar::HintItem> {
        use crate::views::shortcuts_bar::HintItem;
        if self.terminal_has_focus() {
            return vec![
                HintItem::new(crate::key!(']', CONTROL), "tree"),
                HintItem::new(crate::key!(Esc), "editor key"),
            ];
        }
        if self.editor_choice.is_some() {
            return vec![
                HintItem::new(crate::key!(Enter), "open editor"),
                HintItem::new(crate::key!(Esc), "cancel"),
            ];
        }
        if self.focus == Focus::Preview
            && self
                .preview
                .as_ref()
                .is_some_and(|preview| preview.list_state.input_mode().is_some())
        {
            return vec![
                HintItem::new(crate::key!(Enter), "confirm"),
                HintItem::new(crate::key!(Esc), "back to preview"),
            ];
        }
        vec![
            HintItem::new(crate::key!(Tab), "next pane"),
            HintItem::new(crate::key!('e'), "edit"),
            HintItem::new(crate::key!('o'), "default app"),
            HintItem::new(crate::key!('L'), "layout"),
            HintItem::new(crate::key!(Esc), "close files"),
        ]
    }

    pub(crate) fn terminal_has_focus(&self) -> bool {
        self.focused
            && self.focus == Focus::Preview
            && self.editor.is_some()
            && self.editor_choice.is_none()
    }

    pub(crate) fn has_editor(&self) -> bool {
        self.editor.is_some()
    }

    /*
    Purpose: Start the selected editor on a regular file.
    Input: A file path and the selected editor.
    Output: An editor pane, or the launch error.
    */
    pub(crate) fn start_editor(&mut self, path: &Path, editor: Editor) -> anyhow::Result<()> {
        anyhow::ensure!(
            std::fs::metadata(path)?.is_file(),
            "Select a regular file to edit"
        );
        let full_path = dunce::canonicalize(path)?;
        let mut args = Vec::new();
        // macOS also ships Pico as `nano`; it takes the absolute path directly.
        if !matches!(editor, Editor::Nano) {
            args.push(std::ffi::OsStr::new("--"));
        }
        args.push(full_path.as_os_str());
        let terminal = TerminalPane::spawn(
            editor.command(),
            &args,
            &self.root,
            self.hits.preview.width.max(20),
            self.hits.preview.height.max(5),
        )?;
        self.editor = Some(EditorPane {
            terminal,
            path: path.to_path_buf(),
            editor,
        });
        self.focus = Focus::Preview;
        self.focused = true;
        self.notice = None;
        Ok(())
    }

    fn choose_editor(&mut self) {
        if self.editor.is_some() {
            self.focus = Focus::Preview;
        } else if self.focus == Focus::Tree
            && self
                .rows
                .get(self.selected)
                .is_some_and(|row| row.entry.is_directory)
        {
            self.notice = Some("Select a file to edit".into());
        } else if let Some(path) = self.action_path() {
            self.editor_target = Some(path);
            self.editor_choice = Some(self.preferred_editor);
            self.focus = Focus::Preview;
        }
    }

    /*
    Purpose: Move the editor choice or confirm the stored file target.
    Input: The chooser key event.
    Output: A launch request or an updated chooser.
    */
    fn handle_editor_choice(&mut self, key: &KeyEvent) -> BrowserAction {
        let selected = self.editor_choice.expect("editor chooser is open");
        match key.code {
            KeyCode::Esc => {
                self.editor_choice = None;
                self.editor_target = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.editor_choice = Some(selected.saturating_sub(1))
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.editor_choice = Some((selected + 1).min(EDITORS.len() - 1))
            }
            KeyCode::Enter => {
                self.editor_choice = None;
                self.preferred_editor = selected;
                if let Some(path) = self.editor_target.take() {
                    return BrowserAction::Edit {
                        path,
                        editor: EDITORS[selected],
                    };
                }
            }
            _ => {}
        }
        BrowserAction::Changed
    }

    /*
    Purpose: Navigate the visible rows and expand only the requested directory.
    Input: A key event while the tree has focus.
    Output: The new tree selection or a preview request.
    */
    fn handle_tree_key(&mut self, key: &KeyEvent) {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::PageDown => self.select(self.selected + self.hits.tree.height.max(1) as usize),
            KeyCode::PageUp => self.select(
                self.selected
                    .saturating_sub(self.hits.tree.height.max(1) as usize),
            ),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_selected(),
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(row) = self.rows.get(self.selected).cloned() {
                    if row.entry.is_directory {
                        if self.expanded.insert(row.entry.path.clone()) {
                            self.load_directory(&row.entry.path);
                            self.rebuild_rows();
                        } else if self
                            .rows
                            .get(self.selected + 1)
                            .is_some_and(|next| next.depth > row.depth)
                        {
                            self.select(self.selected + 1);
                        }
                    } else {
                        self.open_preview(&row.entry.path);
                        self.focus = Focus::Preview;
                    }
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.collapse_or_parent(),
            _ => {}
        }
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.rows.len().saturating_sub(1));
        let height = self.hits.tree.height.max(1) as usize;
        if self.selected < self.tree_scroll {
            self.tree_scroll = self.selected;
        } else if self.selected >= self.tree_scroll + height {
            self.tree_scroll = self.selected + 1 - height;
        }
    }

    fn action_path(&self) -> Option<PathBuf> {
        if self.focus == Focus::Preview {
            self.editor
                .as_ref()
                .map(|editor| editor.path.clone())
                .or_else(|| self.preview_path.clone())
        } else {
            self.rows
                .get(self.selected)
                .map(|row| row.entry.path.clone())
        }
    }

    /*
    Purpose: Toggle a directory or load the selected file preview.
    Input: The selected tree row and the current pane width.
    Output: The new tree state or a text preview request.
    */
    fn activate_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if row.entry.is_directory {
            if !self.expanded.remove(&row.entry.path) {
                self.expanded.insert(row.entry.path.clone());
                self.load_directory(&row.entry.path);
            }
            self.rebuild_rows();
        } else {
            self.open_preview(&row.entry.path);
            if self.hits.area.width < render::SPLIT_MIN_WIDTH {
                self.focus = Focus::Preview;
            }
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if self.expanded.remove(&row.entry.path) {
            self.rebuild_rows();
        } else if let Some(parent) = row.entry.path.parent()
            && let Some(index) = self.rows.iter().position(|row| row.entry.path == parent)
        {
            self.select(index);
        }
    }

    fn open_preview(&mut self, path: &Path) {
        if self.editor.is_some() {
            self.notice = Some("Exit the current editor before you preview another file".into());
            return;
        }
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_path = Some(path.to_path_buf());
        self.preview = None;
        self.preview_notice = None;
        self.preview_pending = self.send(Request::Preview {
            path: path.to_path_buf(),
            generation: self.preview_generation,
        });
    }

    fn load_directory(&mut self, path: &Path) {
        if self.directories.contains_key(path) || self.pending_directories.contains(path) {
            return;
        }
        if self.send(Request::Directory {
            path: path.to_path_buf(),
            show_all: self.show_all,
            epoch: self.epoch,
        }) {
            self.pending_directories.insert(path.to_path_buf());
        }
    }

    fn send(&mut self, request: Request) -> bool {
        if self.requests.send(request).is_err() {
            self.notice = Some("The file browser worker stopped; close and reopen Files".into());
            false
        } else {
            true
        }
    }

    fn refresh(&mut self) {
        self.notice = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.directories.clear();
        self.pending_directories.clear();
        for path in self.expanded.clone() {
            self.load_directory(&path);
        }
        if let Some(path) = self.preview_path.clone() {
            self.open_preview(&path);
        }
        self.rebuild_rows();
    }

    /*
    Purpose: Flatten the expanded directories and preserve the selected path.
    Input: The current directory cache and expansion set.
    Output: The visible rows and a valid selection index.
    */
    fn rebuild_rows(&mut self) {
        let selected_path = self
            .rows
            .get(self.selected)
            .map(|row| row.entry.path.clone());
        self.rows.clear();
        self.append_row(
            Entry {
                path: self.root.clone(),
                is_directory: true,
                is_symlink: false,
            },
            0,
        );
        self.selected = selected_path
            .and_then(|path| self.rows.iter().position(|row| row.entry.path == path))
            .unwrap_or(self.selected.min(self.rows.len().saturating_sub(1)));
        self.tree_scroll = self.tree_scroll.min(
            self.rows
                .len()
                .saturating_sub(self.hits.tree.height.max(1) as usize),
        );
    }

    /*
    Purpose: Append one entry and the cached children of expanded directories.
    Input: The entry and its depth in the tree.
    Output: Additional visible rows. Symlink entries have no children.
    */
    fn append_row(&mut self, entry: Entry, depth: usize) {
        let path = entry.path.clone();
        self.rows.push(TreeRow { entry, depth });
        if self.expanded.contains(&path)
            && let Some(children) = self.directories.get(&path).cloned()
        {
            for child in children {
                self.append_row(child, depth + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests;
