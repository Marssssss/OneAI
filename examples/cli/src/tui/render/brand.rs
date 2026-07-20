//! Brand line rendering — colorful gradient "OneAI" brand display.
//!
//! Renders the "OneAI" brand name with per-character RGB gradient colors
//! centered in the terminal.
//!
//! - Width < 80: single-line text "O n e A I" with gradient colors + Bold
//! - Width >= 80: 5-line block art for larger brand display
//! - When thinking: spinner + "thinking <elapsed>" appended to the right of the art
//!
//! All width calculations use **visual cell width** (via unicode-width),
//! never byte length — `█` is 3 UTF-8 bytes but 1 terminal cell.
//!
//! **macOS compatibility**: Block art uses **background-colored spaces** instead
//! of foreground `█` characters. On macOS Terminal.app (and some other terminals),
//! `█` may not fill the entire cell width, creating visible gaps between adjacent
//! cells. Background colors always fill the entire cell consistently on all platforms.
//! Each "filled" cell is rendered as a space with bg=BRAND_COLOR, and each "empty"
//! cell is a space with bg=BRAND_BG. This eliminates all gaps.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::super::app::App;
use super::spinner::spinner_char;
use super::super::theme::*;

/// Brand character definitions with their gradient colors.
const BRAND_CHARS: [(char, Color); 5] = [
    ('O', BRAND_O),
    ('n', BRAND_N),
    ('e', BRAND_E),
    ('A', BRAND_A),
    ('I', BRAND_I),
];

/// Block art per-character patterns for "OneAI".
///
/// Each character is defined as 5 row patterns. Each pattern is a sequence
/// of boolean values: true = filled (bg=BRAND_COLOR), false = empty (bg=BRAND_BG).
///
/// Characters are concatenated directly (no gap) during rendering.
/// All patterns are exactly **7 cells** wide.
///
/// Using boolean patterns + bg-colored spaces eliminates the `█` character
/// which has rendering gaps on macOS Terminal.app.
const BLOCK_ART_PATTERNS: [[[bool; 7]; 5]; 5] = [
    // O
    [
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
    ],
    // n
    [
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
    ],
    // e
    [
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
        [false, true,  true,  false, false, false, false],  //  ██
        [false, true,  true,  true,  true,  false, false],  //  ████
        [false, true,  true,  false, false, false, false],  //  ██
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
    ],
    // A
    [
        [false, false, false, true,  true,  false, false],  //    ██
        [false, false, true,  true,  true,  true,  false],  //   ████
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
        [false, true,  true,  false, false, true,  true ],  //  ██  ██
    ],
    // I
    [
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
        [false, false, false, true,  true,  false, false],  //    ██
        [false, false, false, true,  true,  false, false],  //    ██
        [false, false, false, true,  true,  false, false],  //    ██
        [false, true,  true,  true,  true,  true,  true ],  //  ██████
    ],
];

/// Total visual width of the block art: 5 chars × 7 cols = 35 (no gaps).
/// Using bg-colored spaces eliminates the 1-space gap between characters
/// that caused visible discontinuity on macOS.
const BLOCK_ART_VISUAL_WIDTH: usize = 35;

/// Draw the brand line at the top of the TUI.
///
/// Renders "O n e A I" centered with gradient colors.
/// When width >= 80 AND height >= 30, uses 5-line block art.
/// Otherwise, uses single-line text with gradient.
/// When thinking, appends spinner + "thinking <elapsed>" on the right.
pub fn draw_brand(f: &mut Frame, rect: Rect, app: &App) {
    // Only use block art when terminal is large enough (both wide and tall)
    let use_block_art = rect.width >= 80 && f.area().height >= 30;

    if use_block_art {
        draw_block_art_brand(f, rect, app);
    } else {
        draw_text_brand(f, rect, app);
    }
}

/// Compute the visual (cell) width of a Span's content.
fn span_visual_width(span: &Span) -> usize {
    span.content.width()
}

/// Draw the 5-line block art brand using background-colored spaces.
///
/// Instead of foreground `█` characters (which have rendering gaps on macOS),
/// each cell is a space character with the appropriate background color:
/// - "Filled" cells: ` ` with bg=BRAND_COLOR (per-character gradient)
/// - "Empty" cells: ` ` with bg=BRAND_BG (dark background)
///
/// This ensures seamless rendering on all terminals, including macOS.
///
/// Characters are directly abutted (no gap between them), making the total
/// visual width = 5 chars × 7 cols = 35.
fn draw_block_art_brand(f: &mut Frame, rect: Rect, app: &App) {
    let layouts = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(rect);

    // ── Compute consistent left-padding for the block art ──────────────
    let art_padding = if rect.width as usize > BLOCK_ART_VISUAL_WIDTH {
        (rect.width as usize - BLOCK_ART_VISUAL_WIDTH) / 2
    } else {
        0
    };

    // ── Build thinking spans (used only on row 2) ─────────────────────
    let thinking_spans: Vec<Span> = if app.is_thinking {
        let spinner = spinner_char(app.spinner_frame);
        let dur = app
            .work_timer
            .display()
            .map(super::format_work_duration)
            .unwrap_or_default();
        vec![
            Span::styled("  ", Style::default().bg(BRAND_BG)), // gap
            Span::styled(
                format!("{} thinking {}", spinner, dur),
                Style::default().fg(THINKING_COLOR).bg(BRAND_BG),
            ),
        ]
    } else {
        vec![]
    };

    for row_idx in 0..5usize {
        let mut spans = Vec::new();

        // ── Left padding (identical on every row) ──────────────────
        if art_padding > 0 {
            spans.push(Span::styled(
                " ".repeat(art_padding),
                Style::default().bg(BRAND_BG),
            ));
        }

        // ── Block art row using bg-colored spaces ──────────────────
        // Each character is 7 cells wide, directly abutted (no gap).
        // "Filled" cells use bg=character_color, "empty" cells use bg=BRAND_BG.
        // Consecutive cells with the same bg color are grouped into a single Span.
        for (char_idx, (_, color)) in BRAND_CHARS.iter().enumerate() {
            let row_pattern = &BLOCK_ART_PATTERNS[char_idx][row_idx];

            // Group consecutive cells with the same fill state into spans
            let mut i = 0;
            while i < row_pattern.len() {
                let is_filled = row_pattern[i];
                let mut count = 1;
                while i + count < row_pattern.len() && row_pattern[i + count] == is_filled {
                    count += 1;
                }

                let bg_color = if is_filled { *color } else { BRAND_BG };
                spans.push(Span::styled(
                    " ".repeat(count),
                    Style::default().bg(bg_color),
                ));

                i += count;
            }
        }

        // ── Thinking indicator on middle row only ──────────────────
        if row_idx == 2 && !thinking_spans.is_empty() {
            spans.extend(thinking_spans.iter().cloned());
        }

        // ── Right-fill with BG color to full width ─────────────────
        let used_width: usize = spans.iter().map(span_visual_width).sum();
        let remaining = rect.width as usize - used_width;
        if remaining > 0 {
            spans.push(Span::styled(
                " ".repeat(remaining),
                Style::default().bg(BRAND_BG),
            ));
        }

        let line = Line::from(spans);
        let paragraph = Paragraph::new(line).style(Style::default().bg(BRAND_BG));
        f.render_widget(paragraph, layouts[row_idx]);
    }
}

/// Draw the single-line text brand (for narrow terminals).
///
/// Uses visual width (unicode-width) for centering so that Braille
/// spinner characters like ⠋ (3 bytes, 1 cell) are counted correctly.
fn draw_text_brand(f: &mut Frame, rect: Rect, app: &App) {
    // Build the brand spans: "O n e A I" with spaced characters and gradient colors
    let mut brand_spans: Vec<Span> = BRAND_CHARS
        .iter()
        .flat_map(|(ch, color)| {
            vec![
                Span::styled(
                    ch.to_string(),
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "), // spacing between characters
            ]
        })
        .collect();

    // Remove trailing space
    if brand_spans.last().map(|s| s.content == " ").unwrap_or(false) {
        brand_spans.pop();
    }

    // Add thinking indicator on the right if thinking
    if app.is_thinking {
        let spinner = spinner_char(app.spinner_frame);
        let dur = app
            .work_timer
            .display()
            .map(super::format_work_duration)
            .unwrap_or_default();
        brand_spans.push(Span::raw("  "));
        brand_spans.push(Span::styled(
            format!("{} thinking {}", spinner, dur),
            Style::default().fg(THINKING_COLOR),
        ));
    }

    // ── Centering: sum actual visual widths ──────────────────────────
    let total_visual_width: usize = brand_spans.iter().map(span_visual_width).sum();
    let padding = if rect.width as usize > total_visual_width {
        (rect.width as usize - total_visual_width) / 2
    } else {
        0
    };

    // Build the full line with padding + content + remaining fill
    let mut full_spans = Vec::new();
    if padding > 0 {
        full_spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().fg(Color::White).bg(BRAND_BG),
        ));
    }
    full_spans.extend(brand_spans);
    let used = padding + total_visual_width;
    let remaining = rect.width as usize - used;
    if remaining > 0 {
        full_spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().fg(Color::White).bg(BRAND_BG),
        ));
    }

    let line = Line::from(full_spans);
    let paragraph = Paragraph::new(line)
        .style(Style::default().bg(BRAND_BG));

    f.render_widget(paragraph, rect);
}
