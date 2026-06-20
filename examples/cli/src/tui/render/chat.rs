//! Chat area rendering — scrollable viewport for message display.
//!
//! Renders all chat messages in a scrollable viewport with auto-scroll
//! to bottom on new messages. Supports scrollbar for navigation.
//! When search mode is active, displays a search overlay bar and highlights
//! matching messages.
//! Also supports text selection highlight — when the user drags in the
//! chat area, selected lines get a highlight background, and the plain-text
//! content map is built for clipboard copy on release.
//!
//! Performance: Uses a render cache (MessageRenderCache) to avoid re-parsing
//! markdown and re-highlighting code blocks every frame. Only messages whose
//! content, collapsed state, or width has changed are re-rendered.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation},
    Frame,
};

use super::super::app::{App, ChatMessage, ChatRole, content_hash};
use super::message::render_message_lines;
use super::super::theme::*;

/// Draw the chat area.
pub fn draw_chat(f: &mut Frame, rect: Rect, app: &mut App) {
    // If search mode is active, split off 1 row for the search bar at the top
    let (chat_rect, search_rect) = if app.search_mode && rect.height > 2 {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // search bar
                Constraint::Min(0),     // chat area
            ])
            .split(rect);
        (layout[1], layout[0])
    } else {
        (rect, Rect::default())
    };

    // Reserve 1 column for the scrollbar on the right edge.
    // Without this, the scrollbar overwrites the assistant bubble's right │ border.
    let width = (chat_rect.width as usize).saturating_sub(1);

    // Invalidate entire cache on width change (terminal resize)
    app.render_cache.check_width_change(width);

    // Build text lines and plain-text content map from messages using render cache
    let mut lines: Vec<Line> = Vec::new();
    let mut plain_text_lines: Vec<String> = Vec::new(); // for clipboard copy

    for (i, msg) in app.messages.iter().enumerate() {
        let is_collapsed = app.collapsed_ids.contains(&msg.id);
        let is_search_match = app.search_results.contains(&i);
        let is_streaming = app.is_thinking
            && i == app.messages.len() - 1
            && msg.role == ChatRole::Assistant;

        let hash = content_hash(&msg.content);

        // Check if we can use cached lines
        let cached = app.render_cache.entries.get(&msg.id);
        let can_use_cache = !is_streaming
            && cached.is_some()
            && cached.unwrap().content_hash == hash
            && cached.unwrap().was_collapsed == is_collapsed;

        let rendered = if can_use_cache {
            // Use cached lines (clone them since we need owned Vec<Line>)
            cached.unwrap().lines.clone()
        } else {
            // Re-render and cache the result
            let rendered = render_message_lines(msg, is_collapsed, width, app.spinner_frame, app.approval_selected_index);
            app.render_cache.entries.insert(msg.id.clone(), super::super::app::CachedMessage {
                lines: rendered.clone(),
                content_hash: hash,
                was_collapsed: is_collapsed,
            });
            rendered
        };

        // Build plain-text content for each rendered line (for clipboard copy)
        for line in &rendered {
            let plain = line.spans.iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            plain_text_lines.push(plain);
        }

        // If this message matches the search, add a highlight marker
        if is_search_match && app.search_mode {
            let mut highlighted = Vec::new();
            for line in rendered {
                let mut new_spans = vec![
                    Span::styled("🔍 ", Style::default().fg(ratatui::style::Color::Yellow)),
                ];
                new_spans.extend(line.spans.into_iter());
                highlighted.push(Line::from(new_spans));
            }
            lines.extend(highlighted);
        } else {
            lines.extend(rendered);
        }
    }

    // Store plain-text content map for clipboard extraction
    app.line_content = plain_text_lines;

    // Apply text selection highlight
    let scroll_y = app.chat_scroll_y;
    let viewport_height = chat_rect.height as usize;
    if app.text_selection.active && viewport_height > 0 {
        let sel_start = app.text_selection.start_row.min(app.text_selection.end_row) as usize;
        let sel_end = app.text_selection.end_row.max(app.text_selection.start_row) as usize;

        // Convert viewport-relative selection rows to absolute line indices
        let abs_start = scroll_y + sel_start;
        let abs_end = scroll_y + sel_end;

        // Apply highlight background to lines within selection range
        for line_idx in abs_start..(abs_end + 1).min(lines.len()) {
            if let Some(line) = lines.get_mut(line_idx) {
                let highlighted_spans: Vec<Span> = line.spans.iter()
                    .map(|s| {
                        Span::styled(
                            s.content.clone(),
                            s.style.patch(Style::default().bg(SELECTED_BG)),
                        )
                    })
                    .collect();
                *line = Line::from(highlighted_spans);
            }
        }
    }

    let content_height_usize = lines.len();

    // Scroll architecture:
    // - chat_scroll_y: usize — lines scrolled from top (0=top, max=bottom)
    // - user_scrolled: bool — user manually scrolled up, disabling auto-scroll
    // - When user_scrolled is false, auto-scroll to bottom (show latest content)
    // - When user_scrolled is true, keep user's manual scroll position, clamped
    let max_scroll = content_height_usize.saturating_sub(viewport_height);

    let computed_scroll_y = if app.user_scrolled {
        // User manually scrolled — keep their position, clamped to valid range
        app.chat_scroll_y.min(max_scroll)
    } else {
        // Auto-scroll to bottom — show the latest content
        max_scroll
    };
    app.chat_scroll_y = computed_scroll_y;
    // Also re-clamp selection range after scroll adjustment
    // (selection rows are viewport-relative, so they stay valid)

    // Store computed values for scrollbar drag calculation in event handler
    app.content_height = content_height_usize;
    app.last_chat_rect = chat_rect;

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(ratatui::widgets::Borders::NONE))
        .scroll((computed_scroll_y as u16, 0));

    f.render_widget(paragraph, chat_rect);

    // Update scrollbar state
    let mut scrollbar_state = app.scrollbar_state.clone();
    scrollbar_state = scrollbar_state
        .content_length(content_height_usize)
        .viewport_content_length(viewport_height)
        .position(computed_scroll_y);

    // Render scrollbar if content exceeds viewport
    if content_height_usize > viewport_height {
        f.render_stateful_widget(
            Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(SCROLLBAR_THUMB))
                .track_style(Style::default().fg(SCROLLBAR_TRACK)),
            chat_rect,
            &mut scrollbar_state,
        );
    }

    // Render search bar overlay
    if app.search_mode && search_rect.height > 0 {
        draw_search_bar(f, search_rect, app);
    }
}

/// Draw the search bar at the top of the chat area.
fn draw_search_bar(f: &mut Frame, rect: Rect, app: &App) {
    let result_count = app.search_results.len();
    let current = if result_count > 0 {
        format!("({}/{})", app.search_result_index + 1, result_count)
    } else {
        "(no matches)".to_string()
    };

    let search_line = Line::from(vec![
        Span::styled("🔍 ", Style::default().fg(INPUT_PROMPT_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(app.search_query.clone(), Style::default().fg(INPUT_TEXT_COLOR)),
        Span::styled(format!(" {}", current), Style::default().fg(INPUT_HINT_COLOR)),
    ]);

    let paragraph = Paragraph::new(search_line)
        .style(Style::default().bg(BRAND_BG));

    f.render_widget(paragraph, rect);
}

/// Estimate the line count for a message given the viewport width.
#[allow(dead_code)]
fn message_line_count(msg: &ChatMessage, is_collapsed: bool, max_width: usize, spinner_frame: usize) -> usize {
    let lines = render_message_lines(msg, is_collapsed, max_width, spinner_frame, 0);
    lines.len().max(1)
}
