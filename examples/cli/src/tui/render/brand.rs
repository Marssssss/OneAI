//! Brand line rendering — colorful gradient "OneAI" brand display.
//!
//! Renders the "OneAI" brand name with per-character RGB gradient colors
//! centered in the terminal.
//!
//! - Width < 80: single-line text "O n e A I" with gradient colors + Bold
//! - Width >= 80: 5-line ANSI block art for larger brand display
//! - When thinking: spinner + "thinking..." appended to the right of the art
//!
//! All width calculations use **visual cell width** (via unicode-width),
//! never byte length — `█` is 3 UTF-8 bytes but 1 terminal cell.

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
/// Each character is defined as 5 row strings. Characters are concatenated
/// with a 1-space gap during rendering. All patterns are designed
/// to be clearly legible at terminal font sizes.
///
/// Every row of every character is exactly **7 visual columns** wide.
const BLOCK_ART_CHARS: [[&str; 5]; 5] = [
    // O
    [
        " ██████",
        " ██  ██",
        " ██  ██",
        " ██  ██",
        " ██████",
    ],
    // n
    [
        " █████ ",
        " ██ ██ ",
        " ██ ██ ",
        " ██ ██ ",
        " ██ ██ ",
    ],
    // e
    [
        " ██████",
        " ██    ",
        " ████  ",
        " ██    ",
        " ██████",
    ],
    // A
    [
        "   ██  ",
        "  ████ ",
        " ██████",
        " ██  ██",
        " ██  ██",
    ],
    // I
    [
        " ██████",
        "   ██  ",
        "   ██  ",
        "   ██  ",
        " ██████",
    ],
];

/// Total visual width of the block art: 5 chars × 7 cols + 4 gaps × 1 col = 39.
const BLOCK_ART_VISUAL_WIDTH: usize = 39;

/// Draw the brand line at the top of the TUI.
///
/// Renders "O n e A I" centered with gradient colors.
/// When width >= 80 AND height >= 30, uses 5-line block art.
/// Otherwise, uses single-line text with gradient.
/// When thinking, appends spinner + "thinking..." on the right.
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

/// Draw the 5-line ANSI block art brand.
///
/// The block art is always 39 visual columns wide. It is centered uniformly
/// across all 5 rows (same left-padding on every row). When thinking, the
/// spinner + "thinking..." text appears to the **right** of the art on the
/// middle row only, without affecting the art's own alignment.
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
    // Center the 39-col art within the rect; thinking text sits to the
    // right on the middle row and does NOT shift the art itself.
    let art_padding = if rect.width as usize > BLOCK_ART_VISUAL_WIDTH {
        (rect.width as usize - BLOCK_ART_VISUAL_WIDTH) / 2
    } else {
        0
    };

    // ── Build thinking spans (used only on row 2) ─────────────────────
    let thinking_spans: Vec<Span> = if app.is_thinking {
        let spinner = spinner_char(app.spinner_frame);
        vec![
            Span::styled("  ", Style::default().bg(BRAND_BG)), // gap
            Span::styled(
                format!("{} thinking...", spinner),
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

        // ── Block art row ──────────────────────────────────────────
        for (char_idx, (_, color)) in BRAND_CHARS.iter().enumerate() {
            let pattern = BLOCK_ART_CHARS[char_idx][row_idx];
            spans.push(Span::styled(
                pattern.to_string(),
                Style::default().fg(*color).add_modifier(Modifier::BOLD).bg(BRAND_BG),
            ));
            // 1-space gap between characters (except after the last one)
            if char_idx < BRAND_CHARS.len() - 1 {
                spans.push(Span::styled(" ", Style::default().bg(BRAND_BG)));
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
        brand_spans.push(Span::raw("  "));
        brand_spans.push(Span::styled(
            format!("{} thinking...", spinner),
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
