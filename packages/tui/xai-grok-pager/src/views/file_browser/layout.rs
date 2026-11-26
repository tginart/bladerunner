/*
Purpose: Choose where the file browser sits inside the conversation area.
Input: The selected layout, the conversation area, and the scrollbar gutter width.
Output: The browser rectangle and the rectangle left for the conversation.
*/

use ratatui::layout::Rect;

/// Smallest conversation area (cols × rows) that a split may leave.
const MIN_SPLIT_WIDTH: u16 = 80;
const MIN_SPLIT_HEIGHT: u16 = 12;

/// Where the browser sits relative to the conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BrowserLayout {
    #[default]
    Full,
    Top,
    Left,
    Right,
}

impl BrowserLayout {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Full => Self::Top,
            Self::Top => Self::Left,
            Self::Left => Self::Right,
            Self::Right => Self::Full,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Top => "Top",
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }
}

/*
Purpose: Split the conversation area between the browser and the conversation.
Input: The layout, the conversation area, and the scrollbar columns right of it.
Output: The browser area, and the conversation area when the layout is a split.
Method:
    1. Full, or an area too small to split, gives the whole area to the browser.
    2. Top gives the top half to the browser and one gap row.
    3. Left and Right give one half to the browser and one gap column.
Note: Right also keeps the scrollbar gutter clear of the browser.
*/
pub(crate) fn split_area(
    layout: BrowserLayout,
    area: Rect,
    scrollbar_gutter: u16,
) -> (Rect, Option<Rect>) {
    let is_vertical_split = matches!(layout, BrowserLayout::Left | BrowserLayout::Right);
    let is_too_small = match layout {
        BrowserLayout::Full => true,
        BrowserLayout::Top => area.height < MIN_SPLIT_HEIGHT,
        _ => area.width < MIN_SPLIT_WIDTH,
    };
    if is_too_small {
        return (area, None);
    }
    if !is_vertical_split {
        let browser_height = area.height / 2;
        let browser = Rect {
            height: browser_height,
            ..area
        };
        let conversation = Rect {
            y: area.y + browser_height + 1,
            height: area.height - browser_height - 1,
            ..area
        };
        return (browser, Some(conversation));
    }
    let browser_width = area.width / 2;
    let conversation_width = area.width - browser_width - 1;
    if layout == BrowserLayout::Left {
        let browser = Rect {
            width: browser_width,
            ..area
        };
        let conversation = Rect {
            x: area.x + browser_width + 1,
            width: conversation_width,
            ..area
        };
        return (browser, Some(conversation));
    }
    let browser = Rect {
        x: area.right() - browser_width,
        width: browser_width,
        ..area
    };
    let conversation = Rect {
        width: conversation_width.saturating_sub(scrollbar_gutter),
        ..area
    };
    (browser, Some(conversation))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 2,
        y: 3,
        width: 100,
        height: 30,
    };

    #[test]
    fn full_and_small_areas_give_the_browser_everything() {
        assert_eq!(split_area(BrowserLayout::Full, AREA, 0), (AREA, None));
        let narrow = Rect { width: 60, ..AREA };
        assert_eq!(split_area(BrowserLayout::Left, narrow, 0), (narrow, None));
        let short = Rect { height: 8, ..AREA };
        assert_eq!(split_area(BrowserLayout::Top, short, 0), (short, None));
    }

    #[test]
    fn top_split_leaves_the_bottom_half_and_a_gap_row() {
        let (browser, conversation) = split_area(BrowserLayout::Top, AREA, 0);
        let conversation = conversation.unwrap();
        assert_eq!(browser, Rect::new(2, 3, 100, 15));
        assert_eq!(conversation, Rect::new(2, 19, 100, 14));
        assert_eq!(conversation.bottom(), AREA.bottom());
    }

    #[test]
    fn side_splits_leave_a_gap_column_and_clear_the_gutter() {
        let (browser, conversation) = split_area(BrowserLayout::Left, AREA, 2);
        assert_eq!(browser, Rect::new(2, 3, 50, 30));
        assert_eq!(conversation.unwrap(), Rect::new(53, 3, 49, 30));
        let (browser, conversation) = split_area(BrowserLayout::Right, AREA, 2);
        let conversation = conversation.unwrap();
        assert_eq!(browser, Rect::new(52, 3, 50, 30));
        assert_eq!(conversation, Rect::new(2, 3, 47, 30));
        assert!(conversation.right() + 2 < browser.x);
    }

    #[test]
    fn next_cycles_through_every_layout() {
        let mut layout = BrowserLayout::Full;
        let mut seen = Vec::new();
        for _ in 0..4 {
            layout = layout.next();
            seen.push(layout);
        }
        use BrowserLayout::*;
        assert_eq!(seen, [Top, Left, Right, Full]);
    }
}
