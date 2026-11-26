//! Post box of the hub browser: a multiline editor with history and
//! `@handle` / `#channel` completion.
//!
//! The browser owns the layout and the post. This module owns the draft
//! text, the cursor, the sent-post history, and the completion popup.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Clear, StatefulWidgetRef, Widget};
use unicode_width::UnicodeWidthStr;
use xai_ratatui_textarea::{TextArea, TextAreaState};

use crate::render::SafeBuf;
use crate::slash::matcher::FuzzyMatcher;
use crate::theme::Theme;

/// Most text rows the post box shows before it scrolls.
pub const MAX_TEXT_ROWS: u16 = 6;
/// Most rows of the completion popup.
const MAX_COMPLETIONS: usize = 6;
/// Most sent posts that the history keeps.
const MAX_HISTORY: usize = 100;

/// What a key press in the post box asks the browser to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerKey {
    /// Post the draft.
    Send,
    /// Leave the post box. The draft stays.
    Exit,
    /// The draft, the cursor, or the popup changed.
    Edited,
}

/// The words that completion offers.
#[derive(Debug, Clone, Default)]
pub struct CompletionNames {
    /// Member handles, without `@`. `here` is one of them.
    pub handles: Vec<String>,
    /// Channel names, without `#`.
    pub channels: Vec<String>,
}

/// An open completion popup for the word at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Completion {
    /// Byte index of the `@` or `#` that starts the word.
    start: usize,
    /// Candidates with their sigil, such as `@front` or `#ops`.
    items: Vec<String>,
    selected: usize,
}

/// Purpose: Hold the draft of the hub post box.
/// Note: History browse starts with Up on the first line and ends with Down
///     past the newest post, which restores the unsent draft.
#[derive(Debug)]
pub struct HubComposer {
    textarea: TextArea,
    textarea_state: TextAreaState,
    /// Sent posts, oldest first.
    history: Vec<String>,
    /// The history entry that the draft shows, while the person browses.
    history_idx: Option<usize>,
    /// The draft from before the history browse.
    saved_draft: String,
    completion: Option<Completion>,
    /// Text area and popup rows of the last frame, for mouse hits.
    last_text_area: Option<Rect>,
    popup_rows: Vec<Rect>,
    /// Popup row under the mouse pointer.
    hovered_row: Option<usize>,
}

impl Default for HubComposer {
    fn default() -> Self {
        Self {
            textarea: TextArea::new(),
            textarea_state: TextAreaState::default(),
            history: Vec::new(),
            history_idx: None,
            saved_draft: String::new(),
            completion: None,
            last_text_area: None,
            popup_rows: Vec::new(),
            hovered_row: None,
        }
    }
}

impl HubComposer {
    pub fn text(&self) -> &str {
        self.textarea.text()
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.text().is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.textarea.set_text(text);
        self.textarea.set_cursor(text.len());
        self.completion = None;
    }

    pub fn is_completing(&self) -> bool {
        self.completion.is_some()
    }

    /// Purpose: Forget the draft after a post and keep the post in history.
    /// Input: The posted text.
    /// Output: None. The draft is empty and the history browse ends.
    pub fn finish_send(&mut self, posted: &str) {
        if self.history.last().map(String::as_str) != Some(posted) {
            self.history.push(posted.to_string());
        }
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        self.history_idx = None;
        self.saved_draft.clear();
        self.set_text("");
    }

    /// The sent posts, oldest first, so a new browser can keep them.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
    }

    /// Purpose: Apply one key press to the post box.
    /// Input: The key event and the words that completion offers.
    /// Output: What the browser must do next.
    /// Method:
    ///     1. An open popup takes Up, Down, Tab, Enter, and Esc.
    ///     2. Enter sends. Shift+Enter and Alt+Enter go to the editor as a newline.
    ///     3. Esc and Tab leave the post box.
    ///     4. Up on the first line and Down on the last line browse the history.
    ///     5. Other keys go to the editor. Then the popup follows the cursor word.
    pub fn handle_key(&mut self, key: &KeyEvent, names: &CompletionNames) -> ComposerKey {
        if self.completion.is_some()
            && let Some(outcome) = self.handle_completion_key(key)
        {
            return outcome;
        }
        let is_plain = key.modifiers == KeyModifiers::NONE;
        match key.code {
            KeyCode::Enter if is_plain => return ComposerKey::Send,
            KeyCode::Esc | KeyCode::Tab if is_plain => return ComposerKey::Exit,
            KeyCode::Up if is_plain && self.is_cursor_on_first_line() => {
                if self.recall_older() {
                    return ComposerKey::Edited;
                }
            }
            KeyCode::Down if is_plain && self.is_cursor_on_last_line() => {
                if self.recall_newer() {
                    return ComposerKey::Edited;
                }
            }
            _ => {}
        }
        self.textarea.input(*key);
        self.refresh_completion(names);
        ComposerKey::Edited
    }

    /// Insert pasted text at the cursor. Newlines stay.
    pub fn paste(&mut self, text: &str, names: &CompletionNames) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.textarea.insert_str_replacing_selection(&text);
        self.refresh_completion(names);
    }

    /// Purpose: Apply a mouse event to the post box.
    /// Input: The event kind and the screen position.
    /// Output: True when the event hit the popup or the text.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) -> bool {
        let popup_row = self
            .popup_rows
            .iter()
            .position(|rect| rect.contains((col, row).into()));
        match kind {
            MouseEventKind::Moved => {
                let is_changed = popup_row != self.hovered_row;
                self.hovered_row = popup_row;
                is_changed
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(idx) = popup_row {
                    if let Some(completion) = &mut self.completion {
                        completion.selected = idx;
                    }
                    self.accept_completion();
                    return true;
                }
                let Some(area) = self.last_text_area else {
                    return false;
                };
                let Some(pos) =
                    self.textarea
                        .buffer_pos_at_screen(col, row, area, self.textarea_state)
                else {
                    return false;
                };
                self.textarea.clear_selection();
                self.textarea.set_cursor(pos);
                self.completion = None;
                true
            }
            _ => false,
        }
    }

    /// True when a screen cell is on the completion popup.
    pub fn popup_contains(&self, col: u16, row: u16) -> bool {
        self.popup_rows
            .iter()
            .any(|rect| rect.contains((col, row).into()))
    }

    /// Rows of text that the draft needs at `width`, from 1 to MAX_TEXT_ROWS.
    pub fn text_rows(&self, width: u16) -> u16 {
        self.textarea
            .desired_height(width.max(1))
            .clamp(1, MAX_TEXT_ROWS)
    }

    /// Purpose: Draw the draft text.
    /// Input: The buffer, the text area, and whether the box has focus.
    /// Output: The cursor cell when the box has focus.
    pub fn render_text(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        is_focused: bool,
    ) -> Option<(u16, u16)> {
        self.last_text_area = Some(area);
        (&self.textarea).render_ref(area, buf, &mut self.textarea_state);
        if !is_focused {
            return None;
        }
        self.textarea
            .cursor_pos_with_state(area, self.textarea_state)
    }

    /// Purpose: Draw the completion popup above the post box.
    /// Input: The buffer, the post box area, the screen bounds, and the theme.
    /// Output: None. The popup rows become mouse targets.
    pub fn render_popup(&mut self, buf: &mut Buffer, compose: Rect, bounds: Rect, theme: &Theme) {
        self.popup_rows.clear();
        let Some(completion) = &self.completion else {
            return;
        };
        let rows = completion.items.len() as u16;
        let width = completion
            .items
            .iter()
            .map(|item| item.width() as u16 + 4)
            .max()
            .unwrap_or(10)
            .max(16)
            .min(compose.width);
        let height = rows + 2;
        if compose.y < bounds.y + height {
            return;
        }
        let rect = Rect {
            x: compose.x + 1,
            y: compose.y - height,
            width,
            height,
        };
        Clear.render(rect, buf);
        let border = Style::default()
            .fg(theme.prompt_border_active)
            .bg(theme.bg_base);
        let inner_w = rect.width.saturating_sub(2) as usize;
        buf.set_string_safe(rect.x, rect.y, format!("╭{}╮", "─".repeat(inner_w)), border);
        buf.set_string_safe(
            rect.x,
            rect.bottom() - 1,
            format!("╰{}╯", "─".repeat(inner_w)),
            border,
        );
        for (idx, item) in completion.items.iter().enumerate() {
            let y = rect.y + 1 + idx as u16;
            let row = Rect {
                x: rect.x + 1,
                y,
                width: rect.width.saturating_sub(2),
                height: 1,
            };
            let bg = if idx == completion.selected {
                theme.bg_highlight
            } else if Some(idx) == self.hovered_row {
                theme.bg_hover
            } else {
                theme.bg_base
            };
            buf.set_style(row, Style::default().bg(bg));
            buf.set_string_safe(rect.x, y, "│", border);
            buf.set_string_safe(rect.right() - 1, y, "│", border);
            let mut style = Style::default().fg(theme.text_primary).bg(bg);
            if idx == completion.selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            buf.set_string_safe(row.x + 1, y, item, style);
            self.popup_rows.push(row);
        }
    }

    fn handle_completion_key(&mut self, key: &KeyEvent) -> Option<ComposerKey> {
        let completion = self.completion.as_mut()?;
        let last = completion.items.len().saturating_sub(1);
        match key.code {
            KeyCode::Up => completion.selected = completion.selected.saturating_sub(1),
            KeyCode::Down => completion.selected = (completion.selected + 1).min(last),
            KeyCode::Tab | KeyCode::Enter => self.accept_completion(),
            KeyCode::Esc => self.completion = None,
            _ => return None,
        }
        Some(ComposerKey::Edited)
    }

    /// Replace the word at the cursor with the selected candidate and a space.
    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(item) = completion.items.get(completion.selected) else {
            return;
        };
        let cursor = self.textarea.cursor();
        let replacement = format!("{item} ");
        self.textarea
            .replace_range(completion.start..cursor, &replacement);
        self.textarea
            .set_cursor(completion.start + replacement.len());
    }

    /// Open, update, or close the popup for the word at the cursor.
    fn refresh_completion(&mut self, names: &CompletionNames) {
        self.hovered_row = None;
        let text = self.textarea.text();
        let cursor = self.textarea.cursor();
        let Some((start, sigil, query)) = completion_word(text, cursor) else {
            self.completion = None;
            return;
        };
        let pool = match sigil {
            '@' => &names.handles,
            _ => &names.channels,
        };
        let items = rank_names(pool, query, sigil);
        if items.is_empty() || (items.len() == 1 && items[0][1..] == *query) {
            self.completion = None;
            return;
        }
        let selected = match &self.completion {
            Some(old) if old.start == start => old.selected.min(items.len() - 1),
            _ => 0,
        };
        self.completion = Some(Completion {
            start,
            items,
            selected,
        });
    }

    fn is_cursor_on_first_line(&self) -> bool {
        !self.textarea.text()[..self.textarea.cursor()].contains('\n')
    }

    fn is_cursor_on_last_line(&self) -> bool {
        !self.textarea.text()[self.textarea.cursor()..].contains('\n')
    }

    /// Show the next older sent post. Returns false when there is none.
    fn recall_older(&mut self) -> bool {
        let idx = match self.history_idx {
            Some(0) => return false,
            Some(idx) => idx - 1,
            None if self.history.is_empty() => return false,
            None => {
                self.saved_draft = self.textarea.text().to_string();
                self.history.len() - 1
            }
        };
        self.history_idx = Some(idx);
        let text = self.history[idx].clone();
        self.set_text(&text);
        true
    }

    /// Show the next newer sent post, or the saved draft after the newest.
    fn recall_newer(&mut self) -> bool {
        let Some(idx) = self.history_idx else {
            return false;
        };
        let text = if idx + 1 < self.history.len() {
            self.history_idx = Some(idx + 1);
            self.history[idx + 1].clone()
        } else {
            self.history_idx = None;
            std::mem::take(&mut self.saved_draft)
        };
        self.set_text(&text);
        true
    }
}

/// Purpose: Find the `@` or `#` word that ends at the cursor.
/// Input: The draft text and the cursor byte index.
/// Output: The sigil byte index, the sigil, and the typed part after it.
///     None when the cursor is not in such a word.
fn completion_word(text: &str, cursor: usize) -> Option<(usize, char, &str)> {
    let before = text.get(..cursor)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !is_name_char(*c))
        .map(|(idx, _)| idx)?;
    let sigil = before[start..].chars().next()?;
    if !matches!(sigil, '@' | '#') {
        return None;
    }
    let is_word_start = before[..start]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace);
    is_word_start.then(|| (start, sigil, &before[start + sigil.len_utf8()..]))
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.')
}

/// The best matches of `query` in `pool`, with the sigil in front.
fn rank_names(pool: &[String], query: &str, sigil: char) -> Vec<String> {
    let mut matcher = FuzzyMatcher::new();
    matcher
        .rank(pool, query, MAX_COMPLETIONS, |name| name.as_str())
        .into_iter()
        .map(|(idx, _)| format!("{sigil}{}", pool[idx]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(composer: &mut HubComposer, code: KeyCode, names: &CompletionNames) -> ComposerKey {
        composer.handle_key(&KeyEvent::new(code, KeyModifiers::NONE), names)
    }

    fn type_text(composer: &mut HubComposer, text: &str, names: &CompletionNames) {
        for c in text.chars() {
            press(composer, KeyCode::Char(c), names);
        }
    }

    fn names() -> CompletionNames {
        CompletionNames {
            handles: vec!["front".into(), "backend".into(), "here".into()],
            channels: vec!["general".into(), "ops".into()],
        }
    }

    #[test]
    fn enter_sends_and_shift_enter_adds_a_newline() {
        let names = names();
        let mut composer = HubComposer::default();
        type_text(&mut composer, "hi", &names);
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(
            composer.handle_key(&shift_enter, &names),
            ComposerKey::Edited
        );
        type_text(&mut composer, "there", &names);
        assert_eq!(composer.text(), "hi\nthere");
        assert_eq!(
            press(&mut composer, KeyCode::Enter, &names),
            ComposerKey::Send
        );
    }

    #[test]
    fn the_cursor_moves_and_edits_in_place() {
        let names = names();
        let mut composer = HubComposer::default();
        type_text(&mut composer, "helo", &names);
        press(&mut composer, KeyCode::Left, &names);
        type_text(&mut composer, "l", &names);
        assert_eq!(composer.text(), "hello");
    }

    #[test]
    fn up_and_down_browse_history_and_restore_the_draft() {
        let names = names();
        let mut composer = HubComposer::default();
        composer.finish_send("first");
        composer.finish_send("second");
        type_text(&mut composer, "unsent", &names);
        press(&mut composer, KeyCode::Up, &names);
        assert_eq!(composer.text(), "second");
        press(&mut composer, KeyCode::Up, &names);
        assert_eq!(composer.text(), "first");
        press(&mut composer, KeyCode::Up, &names);
        assert_eq!(composer.text(), "first");
        press(&mut composer, KeyCode::Down, &names);
        press(&mut composer, KeyCode::Down, &names);
        assert_eq!(composer.text(), "unsent");
    }

    #[test]
    fn at_word_opens_member_completion_and_tab_accepts() {
        let names = names();
        let mut composer = HubComposer::default();
        type_text(&mut composer, "ask @fr", &names);
        assert!(composer.is_completing());
        assert_eq!(
            press(&mut composer, KeyCode::Tab, &names),
            ComposerKey::Edited
        );
        assert_eq!(composer.text(), "ask @front ");
        assert!(!composer.is_completing());
    }

    #[test]
    fn hash_word_completes_channels_and_esc_closes_the_popup_only() {
        let names = names();
        let mut composer = HubComposer::default();
        type_text(&mut composer, "#o", &names);
        assert!(composer.is_completing());
        assert_eq!(
            press(&mut composer, KeyCode::Esc, &names),
            ComposerKey::Edited
        );
        assert!(!composer.is_completing());
        assert_eq!(
            press(&mut composer, KeyCode::Esc, &names),
            ComposerKey::Exit
        );
        assert_eq!(composer.text(), "#o");
    }

    #[test]
    fn mid_word_sigils_do_not_complete() {
        assert_eq!(completion_word("mail a@fr", 9), None);
        assert_eq!(completion_word("x @fr", 5), Some((2, '@', "fr")));
        assert_eq!(completion_word("@", 1), Some((0, '@', "")));
    }

    #[test]
    fn paste_keeps_newlines() {
        let mut composer = HubComposer::default();
        composer.paste("a\r\nb", &names());
        assert_eq!(composer.text(), "a\nb");
    }
}
