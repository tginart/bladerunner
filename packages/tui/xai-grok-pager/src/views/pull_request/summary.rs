//! Paint a session's PR status over the top right of its scrollback.
//! Only mouse events inside the painted card belong to this surface.

use super::Viewer;
use crate::theme::Theme;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Display {
    #[default]
    Card,
    Chip,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Show,
    Minimize,
    Close,
    Expand,
    Refresh,
}

#[derive(Default)]
pub struct Summary {
    pub display: Display,
    area: Rect,
    buttons: Vec<(Rect, Control)>,
}

impl Summary {
    pub fn button_at(&self, pos: (u16, u16)) -> Option<(Rect, Control)> {
        self.buttons
            .iter()
            .copied()
            .find(|(rect, _)| rect.contains(pos.into()))
    }

    pub fn clear_hits(&mut self) {
        self.area = Rect::default();
        self.buttons.clear();
    }

    pub fn mouse(&self, mouse: &MouseEvent) -> Option<Option<Control>> {
        let point = (mouse.column, mouse.row).into();
        if !self.area.contains(point) {
            return None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            return Some(self.button_at((mouse.column, mouse.row)).map(|(_, c)| c));
        }
        Some(None)
    }

    fn button(
        &mut self,
        buf: &mut Buffer,
        x: u16,
        y: u16,
        label: &str,
        control: Control,
        theme: &Theme,
    ) {
        let rect = Rect::new(x, y, unicode_width::UnicodeWidthStr::width(label) as u16, 1)
            .intersection(self.area);
        Paragraph::new(label)
            .style(Style::default().fg(theme.accent_user).bg(theme.bg_dark))
            .render(rect, buf);
        self.buttons.push((rect, control));
    }
}

impl Viewer {
    /// Purpose: Show PR status without taking space from the conversation or its composer.
    /// Input: The scrollback bounds and current snapshot. Output: The covered rectangle.
    pub fn draw_summary(&mut self, buf: &mut Buffer, bounds: Rect, theme: &Theme) -> Option<Rect> {
        let surface = &mut self.summary;
        surface.clear_hits();
        if surface.display == Display::Closed || bounds.is_empty() {
            return None;
        }
        let badge = self
            .data
            .as_ref()
            .map_or_else(|| "Pull request".to_owned(), |pr| pr.badge.clone());
        if surface.display == Display::Chip || bounds.height < 7 || bounds.width < 25 {
            let label = format!(" {badge} [+] ");
            let width = bounds
                .width
                .min(unicode_width::UnicodeWidthStr::width(label.as_str()) as u16);
            let area = Rect::new(bounds.right() - width, bounds.y, width, 1);
            surface.area = area;
            Clear.render(area, buf);
            let control = if bounds.height < 7 || bounds.width < 25 {
                Control::Expand
            } else {
                Control::Show
            };
            surface.button(buf, area.x, area.y, &label, control, theme);
            return Some(area);
        }
        let width = bounds.width.min(50);
        let area = Rect::new(bounds.right() - width, bounds.y, width, 7);
        surface.area = area;
        Clear.render(area, buf);
        let base = Style::default().fg(theme.text_primary).bg(theme.bg_dark);
        let block = Block::default()
            .borders(Borders::ALL)
            .style(base)
            .border_style(Style::default().fg(theme.gray_dim))
            .title(format!(" {badge} "));
        let inner = block.inner(area);
        block.render(area, buf);
        surface.button(
            buf,
            area.right() - 9,
            area.y,
            "[-]",
            Control::Minimize,
            theme,
        );
        surface.button(buf, area.right() - 5, area.y, "[x]", Control::Close, theme);
        let title = self
            .data
            .as_ref()
            .map_or(self.message.as_str(), |pr| &pr.subject);
        Paragraph::new(title)
            .wrap(Wrap { trim: true })
            .style(base.add_modifier(Modifier::BOLD))
            .render(
                Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 2),
                buf,
            );
        if let Some(pr) = &self.data {
            let files = if pr.changed_files == 1 {
                "file"
            } else {
                "files"
            };
            let stats = Line::from(vec![
                Span::raw(format!("{} {files} · ", pr.changed_files)),
                Span::styled(format!("+{}", pr.additions), base.fg(theme.accent_success)),
                Span::raw(" "),
                Span::styled(format!("−{}", pr.deletions), base.fg(theme.accent_error)),
                Span::raw(format!(" · {}", pr.thread_status)),
            ]);
            Paragraph::new(stats).style(base).render(
                Rect::new(inner.x + 1, inner.y + 2, inner.width.saturating_sub(2), 1),
                buf,
            );
            Paragraph::new(pr.checks.as_str())
                .style(base.fg(theme.text_secondary))
                .render(
                    Rect::new(inner.x + 1, inner.y + 3, inner.width.saturating_sub(2), 1),
                    buf,
                );
        }
        surface.button(
            buf,
            inner.x + 1,
            inner.bottom() - 1,
            "[Fullscreen]",
            Control::Expand,
            theme,
        );
        surface.button(
            buf,
            inner.x + 14,
            inner.bottom() - 1,
            "[Refresh]",
            Control::Refresh,
            theme,
        );
        Some(area)
    }
}
