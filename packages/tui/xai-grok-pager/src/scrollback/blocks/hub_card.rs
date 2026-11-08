//! HubCardBlock — scrollback card for agent hub traffic.
//!
//! Two things in a chat come from a hub: the reply to a `/hub` command, and a
//! wake that delivers hub messages to the agent. Both render as one card: a
//! violet accent bar, a `◆ hub · <title>` header, and a markdown body. The
//! body keeps its links, so a hub name in `/hub list` opens the hub browser.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use xai_grok_tools::implementations::grok_build::agent_hub::Delivery;

use super::markdown_content::MarkdownContent;
use crate::appearance::AppearanceConfig;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockLine, BlockOutput, DisplayMode};
use crate::theme::Theme;

/// Lines before the markdown body: the header and a blank separator.
pub const HUB_CARD_HEADER_LINES: usize = 2;

#[derive(Debug, Clone)]
pub struct HubCardBlock {
    /// The hub name, `hubs`, or `hub`.
    pub title: String,
    /// Short muted text after the title, such as `2 new messages`.
    pub detail: Option<String>,
    content: MarkdownContent,
}

impl HubCardBlock {
    /// A card for the reply to a `/hub` command.
    pub fn command(title: impl Into<String>, markdown: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            detail: None,
            content: MarkdownContent::new(markdown),
        }
    }

    /// Purpose: Make a card for hub messages that woke the agent.
    /// Input: The parsed wake.
    /// Output: A card with one section per message.
    pub fn delivery(delivery: &Delivery) -> Self {
        let title = delivery
            .hub
            .clone()
            .unwrap_or_else(|| "agent messages".to_string());
        let count = delivery.messages.len();
        let detail = if count == 1 {
            "1 new message".to_string()
        } else {
            format!("{count} new messages")
        };
        Self {
            title,
            detail: Some(detail),
            content: MarkdownContent::new(delivery_markdown(delivery)),
        }
    }

    pub fn content(&self) -> &MarkdownContent {
        &self.content
    }

    pub fn copy_text(&self) -> String {
        format!("hub · {}\n\n{}", self.title, self.content.text())
    }
}

/// Lay out the messages of a wake as markdown sections.
fn delivery_markdown(delivery: &Delivery) -> String {
    let mut sections = Vec::new();
    for message in &delivery.messages {
        let place = if message.place == "DM" {
            "DM".to_string()
        } else {
            format!("in {}", message.place)
        };
        let person = if message.is_person { " · person" } else { "" };
        let mut section = format!(
            "**@{}** · {place}{person}\n\n{}",
            message.from, message.text
        );
        match message.image_count {
            0 => {}
            1 => section.push_str("\n\n*1 image*"),
            n => section.push_str(&format!("\n\n*{n} images*")),
        }
        sections.push(section);
    }
    sections.join("\n\n")
}

impl BlockContent for HubCardBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let mut spans = vec![
            Span::styled(
                "◆ hub",
                Style::default()
                    .fg(theme.accent_verify)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", theme.muted()),
            Span::styled(
                self.title.clone(),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(detail) = &self.detail {
            spans.push(Span::styled(format!("  {detail}"), theme.muted()));
        }
        let mut lines = vec![
            BlockLine::styled(Line::from(spans)).with_selection_range(Some(0)),
            BlockLine::separator(Line::from("")),
        ];
        for mut line in self.content.output(ctx.width as usize).lines {
            line.selection_range = Some(0);
            lines.push(line);
        }
        BlockOutput { lines }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        Some(AccentStyle::static_color(Theme::current().accent_verify))
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        true
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Expanded
    }

    fn is_selectable(&self) -> bool {
        true
    }

    fn is_groupable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::implementations::grok_build::agent_hub::DeliveredMessage;

    fn ctx() -> BlockContext {
        BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn plain(block: &HubCardBlock) -> Vec<String> {
        block
            .output(&ctx())
            .lines
            .iter()
            .map(|line| {
                line.content
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn delivery_card_names_the_hub_and_each_sender() {
        let delivery = Delivery {
            hub: Some("team".to_string()),
            messages: vec![
                DeliveredMessage {
                    place: "#general".to_string(),
                    from: "front".to_string(),
                    is_person: false,
                    text: "build is green".to_string(),
                    image_count: 2,
                },
                DeliveredMessage {
                    place: "DM".to_string(),
                    from: "me".to_string(),
                    is_person: true,
                    text: "thanks".to_string(),
                    image_count: 0,
                },
            ],
        };
        let lines = plain(&HubCardBlock::delivery(&delivery));
        assert_eq!(lines[0], "◆ hub · team  2 new messages");
        let body = lines.join("\n");
        assert!(body.contains("@front · in #general"), "{body}");
        assert!(body.contains("build is green"), "{body}");
        assert!(body.contains("2 images"), "{body}");
        assert!(body.contains("@me · DM · person"), "{body}");
    }

    #[test]
    fn command_card_keeps_hub_links() {
        let block = HubCardBlock::command("hubs", "- [team](</hub open team>) · 1 channel");
        assert_eq!(plain(&block)[0], "◆ hub · hubs");
        let has_link = block
            .content()
            .with_hyperlinks(|links| links.iter().any(|link| link.url == "/hub open team"));
        assert!(has_link);
    }
}
