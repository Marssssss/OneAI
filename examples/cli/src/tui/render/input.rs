//! Input area rendering — prompt and input text display with cursor indicator.
//!
//! Renders the input box at the bottom of the TUI with:
//! - Single-line mode: "oneai>" prompt + blinking cursor block
//! - Multi-line vim mode: bordered editor with mode indicator and cursor
//!
//! Long lines are soft-wrapped to the input width (unicode-width aware, CJK =
//! 2 cells) so a long single line never overflows the right border — the root
//! cause of issue #8. `input_visual_line_count` is shared with the layout in
//! `render/mod.rs` so the input area grows with the wrapped line count (capped).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::super::app::{App, InteractionMode};
use super::super::input_mode::{InputMode, VimMode};
use super::super::theme::*;

/// Display width of the prompt prefix `"oneai> "` and its matching continuation
/// indent. All visual input lines reserve this many cells on the left.
const PROMPT_WIDTH: usize = 7;

/// Build the interaction-mode badge shown persistently in the input hint line.
///
/// Always visible (regardless of sidebar state) so the user can see the current
/// cycle mode without pressing Shift+Tab. Returns the badge spans + a separator.
fn mode_badge_spans(app: &App) -> Vec<Span<'static>> {
    let (label, color) = match app.interaction_mode {
        InteractionMode::Normal => ("Normal", LABEL_DIM),
        InteractionMode::AutoAccept => ("⚡Auto", ACTIVE_PARADIGM_COLOR),
        InteractionMode::Plan => ("📋Plan", ACTIVE_PARADIGM_COLOR),
    };
    vec![
        Span::styled("mode:", Style::default().fg(LABEL_DIM)),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(LABEL_DIM)),
    ]
}

/// Compute the number of visual (wrapped) lines the input occupies at the given
/// total input width. Shared with `render/mod.rs` so the layout can size the
/// input area to fit the wrapped content (capped), instead of a fixed 3 rows.
pub fn input_visual_line_count(input: &str, width: usize) -> usize {
    let avail = width.saturating_sub(PROMPT_WIDTH).max(1);
    let mut count = 0usize;
    for logical in input.split('\n') {
        count += wrap_segments(logical, avail).len().max(1);
    }
    count.max(1)
}

/// Break a single (logical) line into wrapped segments each fitting within
/// `max_cols` display columns. Returns `(byte_offset_within_line, slice)` pairs
/// covering the whole line in order. A single character wider than `max_cols`
/// (e.g. CJK in a 1-cell gutter) overflows its cell rather than dropping.
fn wrap_segments(line: &str, max_cols: usize) -> Vec<(usize, &str)> {
    if max_cols == 0 {
        return vec![(0, line)];
    }
    let mut out: Vec<(usize, &str)> = Vec::new();
    let mut seg_start = 0usize;
    let mut col = 0usize;
    for (i, ch) in line.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if col + w > max_cols && i > seg_start {
            out.push((seg_start, &line[seg_start..i]));
            seg_start = i;
            col = 0;
        }
        col += w;
    }
    out.push((seg_start, &line[seg_start..]));
    out
}

/// A wrapped visual line: the text segment (a slice of the full input) and, if
/// the cursor lies on this segment, the byte offset within the segment where the
/// cursor sits (used to split the segment for the cursor block `█`).
struct VisualLine<'a> {
    text: &'a str,
    cursor_local: Option<usize>,
}

/// Wrap the full input (newlines + soft wrap) into visual lines, locating the
/// cursor on exactly one of them. `avail_w` is the per-line content width
/// (excluding the prompt/indent prefix).
fn wrap_input<'a>(input: &'a str, cursor_pos: usize, avail_w: usize) -> Vec<VisualLine<'a>> {
    let cursor_pos = cursor_pos.min(input.len());
    let mut out: Vec<VisualLine<'a>> = Vec::new();
    let mut claimed = false;
    let mut abs = 0usize; // absolute byte offset of the current logical line start
    for logical in input.split('\n') {
        for (seg_off, seg) in wrap_segments(logical, avail_w) {
            let seg_abs_start = abs + seg_off;
            let seg_abs_end = seg_abs_start + seg.len();
            let cursor_local =
                if !claimed && seg_abs_start <= cursor_pos && cursor_pos <= seg_abs_end {
                    claimed = true;
                    Some(cursor_pos - seg_abs_start)
                } else {
                    None
                };
            out.push(VisualLine {
                text: seg,
                cursor_local,
            });
        }
        abs += logical.len() + 1; // +1 for the '\n'
    }
    if out.is_empty() {
        // Only happens for an empty input — ensure one empty line with cursor.
        out.push(VisualLine {
            text: "",
            cursor_local: Some(0),
        });
    }
    out
}

/// Draw the input area.
pub fn draw_input(f: &mut Frame, rect: Rect, app: &App) {
    match app.input_mode {
        InputMode::SingleLine => draw_singleline_input(f, rect, app),
        InputMode::MultiLineVim {
            cursor_position,
            mode,
        } => {
            draw_vim_input(f, rect, app, cursor_position, mode);
        }
    }
}

/// Draw single-line input mode with cursor indicator.
fn draw_singleline_input(f: &mut Frame, rect: Rect, app: &App) {
    let prompt_style = Style::default()
        .fg(INPUT_PROMPT_COLOR)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(INPUT_TEXT_COLOR);
    let cursor_style = Style::default()
        .fg(INPUT_CURSOR_COLOR)
        .add_modifier(Modifier::RAPID_BLINK);
    let prompt = "oneai> ";
    // continuation lines indent to align under the prompt ("oneai> " = 7 cols)
    let cont_indent = "       ";

    let mut input_lines: Vec<Line> = Vec::new();

    if app.is_thinking {
        input_lines.push(Line::from(vec![
            Span::styled(prompt, prompt_style),
            Span::styled("waiting for response...", text_style),
        ]));
    } else {
        // Soft-wrap the input to the input width so a long single line never
        // overflows the right border (issue #8 root cause). Each visual line is
        // a wrapped segment of a logical line (split on '\n' for explicit
        // newlines from Ctrl+Enter / bracketed paste). The cursor block sits on
        // the visual line that holds `input_cursor_pos`, at the right column.
        let input = &app.input;
        let cursor_pos = app.input_cursor_pos.min(input.len());
        let avail_w = (rect.width as usize).saturating_sub(PROMPT_WIDTH).max(1);
        let visual = wrap_input(input, cursor_pos, avail_w);

        for (li, vl) in visual.iter().enumerate() {
            let prefix = if li == 0 { prompt } else { cont_indent };
            let mut spans: Vec<Span> = vec![Span::styled(prefix, prompt_style)];

            if let Some(local) = vl.cursor_local {
                // Cursor is on this visual line — split the segment at the
                // cursor and insert the blinking block.
                let before = &vl.text[..local.min(vl.text.len())];
                let rest = &vl.text[local.min(vl.text.len())..];
                if !before.is_empty() {
                    spans.push(Span::styled(before, text_style));
                }
                if let Some(ch) = rest.chars().next() {
                    spans.push(Span::styled("█", cursor_style));
                    let remaining = &rest[ch.len_utf8()..];
                    spans.push(Span::styled(format!("{}{}", ch, remaining), text_style));
                } else {
                    spans.push(Span::styled("█", cursor_style));
                }
            } else {
                spans.push(Span::styled(vl.text, text_style));
            }
            input_lines.push(Line::from(spans));
        }
        // Guarantee at least one line when the input is empty.
        if input_lines.is_empty() {
            input_lines.push(Line::from(vec![
                Span::styled(prompt, prompt_style),
                Span::styled("█", cursor_style),
            ]));
        }
    }

    let mut hint_spans = mode_badge_spans(app);
    hint_spans.push(Span::styled(
        "[Enter=send Esc=vim Ctrl+C=quit Tab=sidebar ←→=cursor  Shift+Tab=mode]",
        Style::default().fg(INPUT_HINT_COLOR),
    ));
    input_lines.push(Line::from(hint_spans));

    let input_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(INPUT_BORDER));

    let paragraph = Paragraph::new(Text::from(input_lines)).block(input_block);

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
        VimMode::Normal => {
            "[Enter=send Esc=exit vim i=insert h/j/k/l=move x=delete 0/$=line start/end]"
        }
        VimMode::Insert => "[Esc=Normal Enter=newline Ctrl+C=cancel]",
    };

    let mut all_lines = display_lines;
    let mut vim_hint_spans = mode_badge_spans(app);
    vim_hint_spans.push(Span::styled(hints, Style::default().fg(INPUT_HINT_COLOR)));
    all_lines.push(Line::from(vim_hint_spans));

    let paragraph = Paragraph::new(Text::from(all_lines)).block(border_block);

    f.render_widget(paragraph, rect);
}

/// Build display lines with a visible cursor at the given position.
///
/// In Normal mode: cursor is shown as a reversed (highlight) character at the position
/// In Insert mode: cursor is shown as a blinking block █ after the position
fn build_vim_lines_with_cursor(
    input: &str,
    cursor_position: usize,
    mode: VimMode,
) -> Vec<Line<'static>> {
    if input.is_empty() {
        let cursor_char = match mode {
            VimMode::Normal => Span::styled(
                "~",
                Style::default()
                    .fg(INPUT_TEXT_COLOR)
                    .bg(ratatui::style::Color::Yellow),
            ),
            VimMode::Insert => Span::styled(
                "█",
                Style::default()
                    .fg(INPUT_CURSOR_COLOR)
                    .add_modifier(Modifier::RAPID_BLINK),
            ),
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
                spans.push(Span::styled(
                    line_before_cursor.to_string(),
                    Style::default().fg(INPUT_TEXT_COLOR),
                ));
            }

            // Insert cursor indicator
            match mode {
                VimMode::Normal => {
                    // In Normal mode, highlight the character under cursor
                    if let Some(ch) = cursor_char_opt {
                        spans.push(Span::styled(
                            ch.to_string(),
                            Style::default()
                                .fg(ratatui::style::Color::Black)
                                .bg(ratatui::style::Color::Yellow),
                        ));
                    } else {
                        // At end of line — show a space highlight
                        spans.push(Span::styled(
                            " ",
                            Style::default()
                                .fg(ratatui::style::Color::Black)
                                .bg(ratatui::style::Color::Yellow),
                        ));
                    }
                    // Remaining text after cursor character
                    let remaining = if !after.is_empty() && cursor_char_opt.is_some() {
                        let skip = cursor_char_opt.map(|c| c.len_utf8()).unwrap_or(0);
                        &after[skip..]
                    } else {
                        ""
                    };
                    // Get remaining on this line
                    let remaining_this_line = remaining.lines().next().unwrap_or("");
                    if !remaining_this_line.is_empty() {
                        spans.push(Span::styled(
                            remaining_this_line.to_string(),
                            Style::default().fg(INPUT_TEXT_COLOR),
                        ));
                    }
                }
                VimMode::Insert => {
                    // In Insert mode, show blinking block cursor
                    spans.push(Span::styled(
                        "█",
                        Style::default()
                            .fg(INPUT_CURSOR_COLOR)
                            .add_modifier(Modifier::RAPID_BLINK),
                    ));
                    // Remaining text after cursor
                    let remaining = after.lines().next().unwrap_or("");
                    if !remaining.is_empty() {
                        spans.push(Span::styled(
                            remaining.to_string(),
                            Style::default().fg(INPUT_TEXT_COLOR),
                        ));
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
                    Style::default()
                        .fg(ratatui::style::Color::Black)
                        .bg(ratatui::style::Color::Yellow),
                ));
            }
            VimMode::Insert => {
                spans.push(Span::styled(
                    "█",
                    Style::default()
                        .fg(INPUT_CURSOR_COLOR)
                        .add_modifier(Modifier::RAPID_BLINK),
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
            VimMode::Normal => Span::styled(
                " ",
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(ratatui::style::Color::Yellow),
            ),
            VimMode::Insert => Span::styled(
                "█",
                Style::default()
                    .fg(INPUT_CURSOR_COLOR)
                    .add_modifier(Modifier::RAPID_BLINK),
            ),
        };
        display_lines.push(Line::from(cursor));
    }

    display_lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_segments_short_line_stays_one() {
        let segs = wrap_segments("hello", 80);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].1, "hello");
    }

    #[test]
    fn wrap_segments_long_line_breaks_to_width() {
        // 20 chars at width 7 → 3 segments (7,7,6)
        let segs = wrap_segments("abcdefghijklmnopqrst", 7);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].1, "abcdefg");
        assert_eq!(segs[1].1, "hijklmn");
        assert_eq!(segs[2].1, "opqrst");
    }

    #[test]
    fn wrap_segments_cjk_is_two_cells() {
        // CJK chars are width 2; at width 4 only two fit per segment.
        let segs = wrap_segments("你好世界汉字", 4);
        assert_eq!(segs[0].1, "你好");
        assert_eq!(segs[1].1, "世界");
        assert_eq!(segs[2].1, "汉字");
    }

    #[test]
    fn visual_line_count_grows_with_wrapping() {
        // 30-char line at width 23 (avail = 23 - 7 prompt = 16) wraps.
        let long = "a".repeat(40);
        let n = input_visual_line_count(&long, 23);
        assert!(n >= 3, "40 chars / 16 ≈ 3 visual lines, got {n}");
        // empty input = 1 line
        assert_eq!(input_visual_line_count("", 23), 1);
        // explicit newlines each add at least one line
        assert_eq!(input_visual_line_count("a\nb\nc", 80), 3);
    }

    #[test]
    fn wrap_input_locates_cursor_on_correct_segment() {
        // 40-char line, avail 16 → segment 0 = [0,16), seg 1 = [16,32), seg 2 = [32,40).
        let input = "a".repeat(40);
        let vl = wrap_input(&input, 20, 16);
        // cursor at byte 20 → second segment, local 4
        let cur = vl.iter().find(|v| v.cursor_local.is_some()).unwrap();
        assert_eq!(cur.text, &input[16..32]);
        assert_eq!(cur.cursor_local, Some(4));
    }

    #[test]
    fn wrap_input_cursor_at_newline_boundary() {
        // "ab\ncd", cursor at byte 2 (end of first logical line) → on the "ab"
        // segment at its end (local 2), not on the "cd" segment.
        let input = "ab\ncd";
        let vl = wrap_input(input, 2, 80);
        let cur = vl.iter().find(|v| v.cursor_local.is_some()).unwrap();
        assert_eq!(cur.text, "ab");
        assert_eq!(cur.cursor_local, Some(2));
    }

    #[test]
    fn wrap_input_empty_input_has_cursor_on_sole_line() {
        let vl = wrap_input("", 0, 16);
        assert_eq!(vl.len(), 1);
        assert_eq!(vl[0].cursor_local, Some(0));
    }
}
