//! Input area rendering — prompt and input text display with cursor indicator.
//!
//! Renders the input box at the bottom of the TUI with:
//! - Single-line mode: "oneai>" prompt + blinking cursor block
//! - Multi-line vim mode: bordered editor with mode indicator and cursor

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::super::app::App;
use super::super::input_mode::{InputMode, VimMode};
use super::super::theme::*;

/// Draw the input area.
pub fn draw_input(f: &mut Frame, rect: Rect, app: &App) {
    match app.input_mode {
        InputMode::SingleLine => draw_singleline_input(f, rect, app),
        InputMode::MultiLineVim { cursor_position, mode } => {
            draw_vim_input(f, rect, app, cursor_position, mode);
        }
    }
}

/// Draw single-line input mode with cursor indicator.
fn draw_singleline_input(f: &mut Frame, rect: Rect, app: &App) {
    let input_line = if app.is_thinking {
        Line::from(vec![
            Span::styled("oneai> ", Style::default().fg(INPUT_PROMPT_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled("waiting for response...", Style::default().fg(INPUT_TEXT_COLOR)),
        ])
    } else {
        // Split text at cursor position for mid-line cursor display
        let cursor_pos = app.input_cursor_pos.min(app.input.len());
        let before_cursor = &app.input[..cursor_pos];
        let after_cursor = &app.input[cursor_pos..];
        // The character under/after cursor (highlighted in insert style)
        let cursor_char = after_cursor.chars().next();
        let mut spans = vec![
            Span::styled("oneai> ", Style::default().fg(INPUT_PROMPT_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(before_cursor, Style::default().fg(INPUT_TEXT_COLOR)),
        ];
        if let Some(ch) = cursor_char {
            // Show blinking cursor before the character
            spans.push(Span::styled("█", Style::default().fg(INPUT_CURSOR_COLOR).add_modifier(Modifier::RAPID_BLINK)));
            // Show the character after cursor
            let remaining_after_char = &after_cursor[ch.len_utf8()..];
            spans.push(Span::styled(format!("{}{}", ch, remaining_after_char), Style::default().fg(INPUT_TEXT_COLOR)));
        } else {
            // Cursor is at the end — show blinking block
            spans.push(Span::styled("█", Style::default().fg(INPUT_CURSOR_COLOR).add_modifier(Modifier::RAPID_BLINK)));
        }
        Line::from(spans)
    };

    let hint_line = Line::from(Span::styled(
        "[Enter=send Esc=vim Ctrl+C=quit Tab=sidebar ←→=cursor]",
        Style::default().fg(INPUT_HINT_COLOR),
    ));

    let input_text = Text::from(vec![input_line, hint_line]);

    let input_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(INPUT_BORDER));

    let paragraph = Paragraph::new(input_text)
        .block(input_block);

    f.render_widget(paragraph, rect);
}

/// Draw multi-line vim input mode with cursor indicator.
fn draw_vim_input(f: &mut Frame, rect: Rect, app: &App, cursor_position: usize, mode: VimMode) {
    let mode_label = match mode {
        VimMode::Normal => "NORMAL",
        VimMode::Insert => "INSERT",
    };
    let mode_color = match mode {
        VimMode::Normal => ratatui::style::Color::Yellow,
        VimMode::Insert => ratatui::style::Color::Green,
    };

    // Build input lines with cursor indicator at cursor_position
    let input_str = &app.input;
    let display_lines = build_vim_lines_with_cursor(input_str, cursor_position, mode);

    let border_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(INPUT_BORDER))
        .title(Span::styled(
            format!(" {} ", mode_label),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ));

    let hints = match mode {
        VimMode::Normal => "[Enter=发送 Esc=退出vim i=插入 h/j/k/l=移动 x=删除 0/$=行首/行尾]",
        VimMode::Insert => "[Esc=Normal Enter=换行 Ctrl+C=取消]",
    };

    let mut all_lines = display_lines;
    all_lines.push(Line::from(Span::styled(hints, Style::default().fg(INPUT_HINT_COLOR))));

    let paragraph = Paragraph::new(Text::from(all_lines))
        .block(border_block);

    f.render_widget(paragraph, rect);
}

/// Build display lines with a visible cursor at the given position.
///
/// In Normal mode: cursor is shown as a reversed (highlight) character at the position
/// In Insert mode: cursor is shown as a blinking block █ after the position
fn build_vim_lines_with_cursor(input: &str, cursor_position: usize, mode: VimMode) -> Vec<Line<'static>> {
    if input.is_empty() {
        let cursor_char = match mode {
            VimMode::Normal => Span::styled("~", Style::default().fg(INPUT_TEXT_COLOR).bg(ratatui::style::Color::Yellow)),
            VimMode::Insert => Span::styled("█", Style::default().fg(INPUT_CURSOR_COLOR).add_modifier(Modifier::RAPID_BLINK)),
        };
        return vec![Line::from(cursor_char)];
    }

    // Split input at cursor position
    let pos = cursor_position.min(input.len());
    let before = &input[..pos];
    let after = &input[pos..];

    // Determine cursor character (the character at/after cursor position)
    let cursor_char_opt = after.chars().next();

    let lines = before.lines().collect::<Vec<_>>();
    let after_lines = after.lines().collect::<Vec<_>>();

    // Calculate which line the cursor is on
    let cursor_line_idx = before.chars().filter(|c| *c == '\n').count();
    let char_in_line = before.chars().rev().take_while(|c| *c != '\n').count();

    let mut display_lines = Vec::new();

    // Render lines before cursor line
    for (i, line) in lines.iter().enumerate() {
        if i < cursor_line_idx {
            display_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(INPUT_TEXT_COLOR),
            )));
        } else if i == cursor_line_idx {
            // This is the cursor line — split at cursor position within line
            let line_before_cursor = if char_in_line <= line.len() {
                &line[..char_in_line.min(line.len())]
            } else {
                line
            };

            let mut spans = Vec::new();
            if !line_before_cursor.is_empty() {
                spans.push(Span::styled(line_before_cursor.to_string(), Style::default().fg(INPUT_TEXT_COLOR)));
            }

            // Insert cursor indicator
            match mode {
                VimMode::Normal => {
                    // In Normal mode, highlight the character under cursor
                    if let Some(ch) = cursor_char_opt {
                        spans.push(Span::styled(
                            ch.to_string(),
                            Style::default().fg(ratatui::style::Color::Black).bg(ratatui::style::Color::Yellow),
                        ));
                    } else {
                        // At end of line — show a space highlight
                        spans.push(Span::styled(
                            " ",
                            Style::default().fg(ratatui::style::Color::Black).bg(ratatui::style::Color::Yellow),
                        ));
                    }
                    // Remaining text after cursor character
                    let remaining = if after.len() > 0 && cursor_char_opt.is_some() {
                        let skip = cursor_char_opt.map(|c| c.len_utf8()).unwrap_or(0);
                        &after[skip..]
                    } else {
                        ""
                    };
                    // Get remaining on this line
                    let remaining_this_line = remaining.lines().next().unwrap_or("");
                    if !remaining_this_line.is_empty() {
                        spans.push(Span::styled(remaining_this_line.to_string(), Style::default().fg(INPUT_TEXT_COLOR)));
                    }
                }
                VimMode::Insert => {
                    // In Insert mode, show blinking block cursor
                    spans.push(Span::styled(
                        "█",
                        Style::default().fg(INPUT_CURSOR_COLOR).add_modifier(Modifier::RAPID_BLINK),
                    ));
                    // Remaining text after cursor
                    let remaining = after.lines().next().unwrap_or("");
                    if !remaining.is_empty() {
                        spans.push(Span::styled(remaining.to_string(), Style::default().fg(INPUT_TEXT_COLOR)));
                    }
                }
            }

            display_lines.push(Line::from(spans));
        }
    }

    // Handle case where cursor is on a line that doesn't exist in `lines` (e.g., at end of input)
    if cursor_line_idx >= lines.len() {
        let mut spans = Vec::new();
        match mode {
            VimMode::Normal => {
                spans.push(Span::styled(
                    " ",
                    Style::default().fg(ratatui::style::Color::Black).bg(ratatui::style::Color::Yellow),
                ));
            }
            VimMode::Insert => {
                spans.push(Span::styled(
                    "█",
                    Style::default().fg(INPUT_CURSOR_COLOR).add_modifier(Modifier::RAPID_BLINK),
                ));
            }
        }
        display_lines.push(Line::from(spans));
    }

    // Render remaining lines after cursor line
    for (i, line) in after_lines.iter().enumerate() {
        // Skip first line of `after_lines` if cursor was already rendered on it
        if i > 0 || (cursor_line_idx < lines.len() && lines[cursor_line_idx].len() > char_in_line) {
            display_lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(INPUT_TEXT_COLOR),
            )));
        }
    }

    // If display is still empty, add at least one cursor line
    if display_lines.is_empty() {
        let cursor = match mode {
            VimMode::Normal => Span::styled(" ", Style::default().fg(ratatui::style::Color::Black).bg(ratatui::style::Color::Yellow)),
            VimMode::Insert => Span::styled("█", Style::default().fg(INPUT_CURSOR_COLOR).add_modifier(Modifier::RAPID_BLINK)),
        };
        display_lines.push(Line::from(cursor));
    }

    display_lines
}
