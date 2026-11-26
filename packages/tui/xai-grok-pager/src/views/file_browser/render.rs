/*
Purpose: Draw the file tree and read-only preview inside the conversation area.
Input: The browser state, frame buffer, and available rectangle.
Output: The frame cells and mouse hit rectangles for the current frame.
*/

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, StatefulWidget, Widget};

use crate::theme::Theme;
use crate::views::file_search::line_viewer::LineViewerState;
use crate::views::list_pane::ListPane;

use super::{FileBrowserState, Focus, HitAreas};

pub(super) const SPLIT_MIN_WIDTH: u16 = 64;

/*
Purpose: Draw a split browser or one focused pane on a narrow terminal.
Input: The view state, buffer, and available rectangle.
Output: The frame cells and current mouse hit areas.
*/
pub(super) fn render(browser: &mut FileBrowserState, buf: &mut Buffer, area: Rect) {
    browser.hits = HitAreas {
        area,
        ..HitAreas::default()
    };
    if area.width < 8 || area.height < 5 {
        return;
    }
    let theme = Theme::current();
    Clear.render(area, buf);
    buf.set_style(
        area,
        Style::default().fg(theme.text_primary).bg(theme.bg_base),
    );
    let title = format!("Files — {}", browser.root.display());
    buf.set_stringn(
        area.x,
        area.y,
        title,
        area.width as usize,
        Style::default().fg(theme.path),
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 3);
    let is_split = area.width >= SPLIT_MIN_WIDTH;
    let tree_width = if is_split {
        (area.width / 3).clamp(22, 36)
    } else {
        area.width
    };
    if is_split || browser.focus == Focus::Tree {
        render_tree(
            browser,
            buf,
            Rect::new(body.x, body.y, tree_width, body.height),
            &theme,
        );
    }
    if is_split || browser.focus == Focus::Preview {
        let preview = if is_split {
            Rect::new(
                body.x + tree_width,
                body.y,
                body.width - tree_width,
                body.height,
            )
        } else {
            body
        };
        render_preview(browser, buf, preview, &theme);
    }
    let default_hint = if browser.editor.is_some() {
        "Ctrl+]: focus tree · Tab: focus editor · Exit editor normally to return to preview"
    } else {
        "Tab: tree → preview → prompt · Enter: open · ←/→: collapse/expand"
    };
    let notice = browser.notice.as_deref().unwrap_or(default_hint);
    buf.set_stringn(
        area.x,
        area.bottom() - 2,
        notice,
        area.width as usize,
        Style::default().fg(theme.gray),
    );
    render_buttons(browser, buf, area, &theme);
}

/*
Purpose: Draw only the tree rows that fit the current viewport.
Input: The tree state and its frame rectangle.
Output: Styled rows and a tree content hit area.
*/
fn render_tree(browser: &mut FileBrowserState, buf: &mut Buffer, area: Rect, theme: &Theme) {
    let focused = browser.focused && browser.focus == Focus::Tree;
    let border = Block::default()
        .borders(Borders::ALL)
        .title(" Tree ")
        .border_style(Style::default().fg(if focused { theme.path } else { theme.gray_dim }));
    let inner = border.inner(area);
    border.render(area, buf);
    browser.hits.tree = inner;
    browser.tree_scroll = browser
        .tree_scroll
        .min(browser.rows.len().saturating_sub(inner.height as usize));
    for (line, row) in browser
        .rows
        .iter()
        .skip(browser.tree_scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        let index = browser.tree_scroll + line;
        let marker = if row.entry.is_directory {
            if browser.pending_directories.contains(&row.entry.path) {
                "…"
            } else if browser.expanded.contains(&row.entry.path) {
                "▾"
            } else {
                "▸"
            }
        } else if row.entry.is_symlink {
            "@"
        } else {
            " "
        };
        let name = row
            .entry
            .path
            .file_name()
            .unwrap_or(row.entry.path.as_os_str())
            .to_string_lossy();
        let label = format!("{}{marker} {name}", "  ".repeat(row.depth.min(32)));
        let style = if index == browser.selected {
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.bg_visual)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(if row.entry.is_directory {
                theme.path
            } else {
                theme.text_primary
            })
        };
        let y = inner.y + line as u16;
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        buf.set_stringn(inner.x, y, label, inner.width as usize, style);
    }
}

/*
Purpose: Draw the editor choice, terminal, or read-only file preview.
Input: The current preview state and its frame rectangle.
Output: The pane content and its mouse hit area.
*/
fn render_preview(browser: &mut FileBrowserState, buf: &mut Buffer, area: Rect, theme: &Theme) {
    let focused = browser.focused && browser.focus == Focus::Preview;
    let path = browser
        .editor
        .as_ref()
        .map(|editor| &editor.path)
        .or(browser.editor_target.as_ref())
        .or(browser.preview_path.as_ref());
    let title = path
        .map(|path| {
            path.strip_prefix(&browser.root)
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .unwrap_or_else(|| "Preview".into());
    let mode = browser
        .editor
        .as_ref()
        .map(|editor| editor.editor.command())
        .unwrap_or("read only");
    let border = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} · {mode} "))
        .border_style(Style::default().fg(if focused { theme.path } else { theme.gray_dim }));
    let mut inner = border.inner(area);
    border.render(area, buf);
    if let Some(selected) = browser.editor_choice {
        browser.hits.preview = inner;
        let labels = [
            "Choose an installed editor (↑/↓, Enter; Esc cancels)",
            "Neovim (nvim)",
            "Vim (vim)",
            "nano",
        ];
        for (index, label) in labels.iter().enumerate().take(inner.height as usize) {
            let style = if index == selected + 1 {
                Style::default().fg(theme.text_primary).bg(theme.bg_visual)
            } else {
                Style::default().fg(theme.gray)
            };
            buf.set_stringn(
                inner.x,
                inner.y + index as u16,
                label,
                inner.width as usize,
                style,
            );
            if index > 0 {
                browser.hits.editor_choices[index - 1] =
                    Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            }
        }
        return;
    }
    if let Some(editor) = &mut browser.editor {
        browser.hits.preview = inner;
        if let Err(error) = editor.terminal.resize(inner.width, inner.height) {
            browser.notice = Some(format!("Could not resize editor: {error}"));
        }
        editor.terminal.render(buf, inner, focused);
        return;
    }
    if let Some(notice) = &browser.preview_notice
        && inner.height > 0
    {
        buf.set_stringn(
            inner.x,
            inner.bottom() - 1,
            notice,
            inner.width as usize,
            Style::default().fg(theme.gray),
        );
        inner.height -= 1;
    }
    browser.hits.preview = inner;
    if let Some(preview) = &mut browser.preview {
        preview.prepare_layout(inner.width, inner.height);
        let pane = ListPane::new(&preview.lines)
            .focused(focused)
            .style(LineViewerState::list_pane_style());
        StatefulWidget::render(pane, inner, buf, &mut preview.list_state);
    } else if inner.height > 0 {
        let label = if browser.preview_pending {
            "Loading preview…"
        } else if browser.preview_path.is_none() {
            "Select a file, then press Enter or click it"
        } else {
            "Use Open in Default App to view this file"
        };
        buf.set_stringn(
            inner.x,
            inner.y,
            label,
            inner.width as usize,
            Style::default().fg(theme.gray),
        );
    }
}

/*
Purpose: Draw the actions that fit and reserve space for the close button.
Input: The browser state and the footer rectangle.
Output: Button labels and hit areas for visible buttons.
*/
fn render_buttons(browser: &mut FileBrowserState, buf: &mut Buffer, area: Rect, theme: &Theme) {
    let mut x = area.x;
    let y = area.bottom() - 1;
    let pointer = browser.pointer;
    let close_label = "[Close]";
    let right = if area.width >= close_label.len() as u16 {
        let close_x = area.right() - close_label.len() as u16;
        let close = Rect::new(close_x, y, close_label.len() as u16, 1);
        buf.set_string(close_x, y, close_label, button_style(close, pointer, theme));
        browser.hits.close = close;
        close_x.saturating_sub(2)
    } else {
        area.right()
    };
    let all_label = if browser.show_all {
        "[Hide ignored]"
    } else {
        "[All files]"
    };
    for (label, hit) in [
        ("[Open in Default App]", &mut browser.hits.open),
        ("[Edit]", &mut browser.hits.edit),
        ("[Refresh]", &mut browser.hits.refresh),
        (all_label, &mut browser.hits.show_all),
    ] {
        let width = label.len() as u16;
        if x + width > right {
            continue;
        }
        let rect = Rect::new(x, y, width, 1);
        buf.set_string(x, y, label, button_style(rect, pointer, theme));
        *hit = rect;
        x += width + 2;
    }
}

/// The style of a footer button: underlined and bright under the pointer.
fn button_style(rect: Rect, pointer: Option<(u16, u16)>, theme: &Theme) -> Style {
    let style = Style::default().fg(theme.command);
    match pointer {
        Some(point) if rect.contains(point.into()) => style
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        _ => style,
    }
}
