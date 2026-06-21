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
    widgets::{Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation},
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

    let viewport_height = chat_rect.height as usize;

    // ── Pass 1: ensure every message is rendered+cached and record each one's
    //    line-count offset so we can compute the total content height and later
    //    locate the visible window. This is O(messages) — it never clones the
    //    cached lines, only reads `.len()`.
    //
    //    (The streaming/last-assistant message is re-rendered each frame since
    //    its content mutates; everything else hits the cache.)
    let mut seg_offsets: Vec<(usize, usize)> = Vec::with_capacity(app.messages.len());
    let mut total_height = 0usize;

    for (i, msg) in app.messages.iter().enumerate() {
        let is_collapsed = app.collapsed_ids.contains(&msg.id);
        let is_streaming = app.is_thinking
            && i == app.messages.len() - 1
            && msg.role == ChatRole::Assistant;
        let hash = content_hash(&msg.content);

        let cached = app.render_cache.entries.get(&msg.id);
        let can_use_cache = !is_streaming
            && cached.is_some()
            && cached.unwrap().content_hash == hash
            && cached.unwrap().was_collapsed == is_collapsed;

        if !can_use_cache {
            let rendered = render_message_lines(
                msg,
                is_collapsed,
                width,
                app.spinner_frame,
                app.approval_selected_index,
            );
            app.render_cache.entries.insert(
                msg.id.clone(),
                super::super::app::CachedMessage {
                    lines: rendered,
                    content_hash: hash,
                    was_collapsed: is_collapsed,
                },
            );
        }

        let len = app
            .render_cache
            .entries
            .get(&msg.id)
            .map(|c| c.lines.len())
            .unwrap_or(0);
        seg_offsets.push((total_height, len));
        total_height += len;
    }

    let content_height_usize = total_height;

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

    // Store computed values for scrollbar drag calculation in event handler
    app.content_height = content_height_usize;
    app.last_chat_rect = chat_rect;

    // ── Pass 2: collect ONLY the lines inside the visible window.
    //
    //    This is the key performance fix for long histories / large model
    //    outputs: instead of cloning the entire cached line set every frame and
    //    letting `Paragraph::scroll` skip the off-screen rows (which still pays
    //    the full clone + alloc cost), we slice to exactly the visible range.
    //    Cost drops from O(total_lines) to O(visible_lines).
    let visible_start = computed_scroll_y;
    let visible_end = (computed_scroll_y + viewport_height).min(content_height_usize);

    let mut lines: Vec<Line> =
        Vec::with_capacity(visible_end.saturating_sub(visible_start) + 2);

    if visible_end > visible_start {
        for (i, msg) in app.messages.iter().enumerate() {
            let (seg_start, seg_len) = seg_offsets[i];
            if seg_len == 0 {
                continue;
            }
            let seg_end = seg_start + seg_len;
            // Skip messages entirely above or below the viewport.
            if seg_end <= visible_start || seg_start >= visible_end {
                continue;
            }

            let cached = match app.render_cache.entries.get(&msg.id) {
                Some(c) => c,
                None => continue,
            };

            // Clamp the message's line range to the visible window.
            let line_start = visible_start.saturating_sub(seg_start);
            let line_end = visible_end.saturating_sub(seg_start).min(seg_len);
            let visible_slice = &cached.lines[line_start..line_end];

            // Search-match highlight is applied per visible line only.
            let is_search_match = app.search_mode && app.search_results.contains(&i);
            if is_search_match {
                for line in visible_slice {
                    let mut new_spans =
                        vec![Span::styled("🔍 ", Style::default().fg(ratatui::style::Color::Yellow))];
                    new_spans.extend(line.spans.iter().cloned());
                    lines.push(Line::from(new_spans));
                }
            } else {
                lines.extend(visible_slice.iter().cloned());
            }
        }
    }

    // ── Render ───────────────────────────────────────────────────────────────
    //
    // `Clear` is essential for scroll correctness: ratatui's `Paragraph` only
    // writes the graphemes each line actually contains — it does NOT clear the
    // trailing cells beyond a line's content, and `buf.set_style` updates only
    // the style, not the glyph. Because `Terminal::draw` diffs the new buffer
    // against the previous frame and emits only changed cells, any cell the
    // Paragraph didn't touch retains its previous-frame glyph.
    //
    // Many rendered lines are shorter than the viewport (blank separators,
    // system/error rows, collapse hints, …). When scrolling, such a short line
    // moves into a row previously held by a longer line, leaving the longer
    // line's trailing glyphs on screen — the "scrolled-out content still shows
    // in place" ghosting. Clearing the whole chat rect first guarantees every
    // cell is reset to blank each frame, so the diff never sees stale glyphs.
    //
    // This is cheap: `Clear` writes to the in-memory buffer; the terminal only
    // receives cells whose final value differs from the previous frame (which
    // during a scroll is exactly the set of cells that genuinely changed).
    f.render_widget(Clear, chat_rect);

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(ratatui::widgets::Borders::NONE))
        .scroll((0, 0));

    f.render_widget(paragraph, chat_rect);

    // Update scrollbar state
    let mut scrollbar_state = app.scrollbar_state.clone();
    scrollbar_state = scrollbar_state
        .content_length(content_height_usize)
        .viewport_content_length(viewport_height)
        .position(computed_scroll_y);

    // Render scrollbar if content exceeds viewport.
    // Use ┃ (heavy vertical bar) for thumb instead of default █.
    // On macOS Terminal.app, █ may not fill the full cell width, creating
    // visible gaps between consecutive thumb cells. ┃ renders consistently
    // as a thick continuous vertical bar on all terminals.
    if content_height_usize > viewport_height {
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_symbol("┃")
                .track_symbol(Some("╎"))
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
