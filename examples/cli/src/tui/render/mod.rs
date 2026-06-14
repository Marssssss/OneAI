//! Render module — draws the full TUI layout.
//!
//! Layout structure:
//! ```
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ 品牌行 (1行)                                                      │
//! ├──────────┬───────────────────────────────────────────────────────┤
//! │ 侧栏24列 │  聊天区域 (Min)                                        │
//! │          ├───────────────────────────────────────────────────────┤
//! │          │  输入区 (3行)                                            │
//! └──────────┴───────────────────────────────────────────────────────┘
//! ```

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem},
    Frame,
};

use super::app::App;
use super::theme::*;

pub mod brand;
pub mod chat;
pub mod context_bar;
pub mod input;
pub mod markdown;
pub mod message;
pub mod sidebar;
pub mod spinner;
pub mod approval;
pub mod diff;

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &mut App) {
    let total_size = f.area();

    // Determine brand line height: 5 lines for block art (large terminal), 1 line for text
    let brand_lines = if total_size.width >= 80 && total_size.height >= 30 { 5 } else { 1 };
    let context_bar_lines = if !app.show_sidebar { 1 } else { 0 };

    let outer_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if context_bar_lines > 0 {
            vec![
                Constraint::Length(brand_lines),         // brand line (1 or 3)
                Constraint::Length(context_bar_lines),    // context bar
                Constraint::Min(0),                       // main content
            ]
        } else {
            vec![
                Constraint::Length(brand_lines),
                Constraint::Min(0),
            ]
        })
        .split(total_size);

    let brand_rect = outer_layout[0];
    let context_bar_rect = if context_bar_lines > 0 {
        outer_layout[1]
    } else {
        Rect::default()
    };
    let content_rect = if context_bar_lines > 0 {
        outer_layout[2]
    } else {
        outer_layout[1]
    };

    // Draw brand line
    brand::draw_brand(f, brand_rect, app);

    // Draw context bar when sidebar is hidden
    if !app.show_sidebar && context_bar_rect.height > 0 {
        context_bar::draw_context_bar(f, context_bar_rect, app);
    }

    // Main content: sidebar | (chat + input)
    let main_layout = if app.show_sidebar {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(0)])
            .split(content_rect)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(0), Constraint::Min(0)])
            .split(content_rect)
    };

    let sidebar_rect = main_layout[0];
    let right_rect = main_layout[1];

    // Right panel: chat | input
    let panel_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),      // chat area
            Constraint::Length(3),   // input box (2 lines + border)
        ])
        .split(right_rect);

    draw_sidebar(f, sidebar_rect, app);
    chat::draw_chat(f, panel_layout[0], app);
    input::draw_input(f, panel_layout[1], app);

    // Draw command autocomplete popup (if active)
    if app.command_autocomplete && !app.input.is_empty() && app.input.starts_with('/') {
        draw_command_popup(f, panel_layout[0], panel_layout[1], app);
    }
}

/// Draw the sidebar (delegates to sidebar module).
fn draw_sidebar(f: &mut Frame, rect: Rect, app: &App) {
    sidebar::draw_sidebar(f, rect, app);
}

/// Draw command autocomplete popup.
///
/// Shows a floating list of matching commands at the bottom of the chat area,
/// just above the input box. The selected command is highlighted with a
/// prominent ▸ indicator and reversed (highlight) style for maximum visibility.
fn draw_command_popup(f: &mut Frame, chat_rect: Rect, _input_rect: Rect, app: &App) {
    let suggestions = app.get_command_suggestions();
    if suggestions.is_empty() {
        return;
    }

    // Clamp selected index to valid range
    let selected = app.command_autocomplete_index.min(suggestions.len() - 1);

    // Show at most 8 suggestions at a time
    let max_visible = 8;
    let total_count = suggestions.len();
    let visible_count = total_count.min(max_visible);
    let popup_height = visible_count as u16 + 2; // +2 for border

    // Calculate scroll offset so the selected item is always visible
    let scroll_offset = if selected >= max_visible {
        selected - max_visible + 1
    } else {
        0
    };

    // Position the popup at the bottom of the chat area, above the input box
    let popup_rect = Rect {
        x: chat_rect.x + 2,
        y: chat_rect.y + chat_rect.height.saturating_sub(popup_height),
        width: 50.min(chat_rect.width.saturating_sub(4)),
        height: popup_height.min(chat_rect.height),
    };

    // Clear the area before rendering (so it floats above chat content)
    f.render_widget(Clear, popup_rect);

    // Build list items with prominent selection indicator
    let items: Vec<ListItem> = suggestions.iter().enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(i, (cmd, desc))| {
            let is_selected = i == selected;

            // Selected item: ▸ indicator + bold cmd + reversed background
            // Non-selected item: blank prefix + normal cmd + dim desc
            let indicator = if is_selected { "▸ " } else { "  " };
            let indicator_style = if is_selected {
                Style::default().fg(ratatui::style::Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ratatui::style::Color::DarkGray)
            };
            let cmd_style = if is_selected {
                Style::default().fg(INPUT_PROMPT_COLOR).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(INPUT_TEXT_COLOR)
            };
            let desc_style = if is_selected {
                Style::default().fg(INPUT_HINT_COLOR).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ratatui::style::Color::DarkGray)
            };

            ListItem::new(Line::from(vec![
                Span::styled(indicator, indicator_style),
                Span::styled(format!("{} ", cmd), cmd_style),
                Span::styled(desc.to_string(), desc_style),
            ]))
        })
        .collect();

    // Show scroll indicator if there are more items than visible
    let title_suffix = if total_count > max_visible {
        format!(" ({}/{})", selected + 1, total_count)
    } else {
        String::new()
    };

    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(INPUT_BORDER))
            .title(Span::styled(
                format!(" Commands{} ", title_suffix),
                Style::default().fg(INPUT_PROMPT_COLOR).add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(BRAND_BG)));

    f.render_widget(list, popup_rect);
}
