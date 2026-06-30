//! Approval card rendering — displays approval request UI for high-risk tools.
//!
//! Renders a yellow warning card showing:
//! - Tool name and permission/risk level
//! - Arguments/justification details
//! - Y/N/M/A response options
//! - Adaptive width based on terminal width

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::super::app::ApprovalPendingState;
use super::super::theme::*;

/// Draw an approval card in the chat area.
///
/// Renders a yellow warning card with adaptive width.
/// When rendered standalone (outside the chat bubble system),
/// this provides a bordered card with full details.
#[allow(dead_code)]
pub fn draw_approval_card(f: &mut Frame, rect: Rect, pending: &ApprovalPendingState) {
    let request = &pending.request;

    let perm_label = request.permission_level.map(|p| format!("{:?}", p))
        .unwrap_or_else(|| format!("{:?}", request.risk_level));

    let content = format!(
        "Tool: {} ({})\n{}\n\n[Y] Approve  [N] Deny  [M] Modify  [A] Always for session",
        request.tool_name,
        perm_label,
        request.justification,
    );

    let lines: Vec<Line<'static>> = content.lines().map(|line| {
        Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(APPROVAL_COLOR),
        ))
    }).collect();

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(APPROVAL_BORDER))
            .title(Span::styled(
                " ⚠️ Approval Required ",
                Style::default().fg(APPROVAL_COLOR).add_modifier(Modifier::BOLD),
            )));

    f.render_widget(paragraph, rect);
}

/// Render an approval message as chat lines with adaptive width.
///
/// This is used inside the message bubble rendering system (message.rs).
/// The card adapts its width based on `max_width` parameter.
/// The selected option is highlighted with bold styling.
pub fn render_approval_card_inline(content: &str, max_width: usize, selected_index: usize) -> Vec<Line<'static>> {
    let inner_width = max_width.saturating_sub(4); // border padding

    let mut lines = Vec::new();

    // Top border: ┌─ ⚠️ Approval Required ───┐
    // Structure: "┌─ " (3) + title (title_visual_width) + "─"×fill_len + "┐" (1)
    // Total = inner_width + 4, so fill_len = inner_width - title_visual_width
    let title = "⚠️ Approval Required ";
    let title_visual_width = title.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(APPROVAL_BORDER)),
        Span::styled(title, Style::default().fg(APPROVAL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(APPROVAL_BORDER)),
        Span::styled("┐", Style::default().fg(APPROVAL_BORDER)),
    ]));

    // Content lines with adaptive wrapping
    for content_line in content.lines() {
        // Wrap long lines within the card
        let wrapped = wrap_line(content_line, inner_width);
        for wrapped_line in wrapped {
            let line_width = wrapped_line.width();
            // Total = "│ " (2) + content + padding + " │" (2) = inner_width + 4
            // padding = inner_width - line_width
            let padding = inner_width.saturating_sub(line_width);
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(APPROVAL_BORDER)),
                Span::styled(wrapped_line, Style::default().fg(APPROVAL_COLOR)),
                Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)),
                Span::styled(" │", Style::default().fg(APPROVAL_BORDER)),
            ]));
        }
    }

    // Action buttons row with selection highlight
    // 0=Y Approve, 1=N Deny, 2=M Modify, 3=A Always
    let options = [
        ("[Y] Approve", APPROVAL_COLOR),
        ("[N] Deny", ratatui::style::Color::Rgb(255, 80, 80)),  // Red for deny
        ("[M] Modify", ratatui::style::Color::Rgb(160, 160, 200)), // Blue-ish for modify
        ("[A] Always", APPROVAL_COLOR),
    ];

    let mut action_spans = vec![
        Span::styled("│ ", Style::default().fg(APPROVAL_BORDER)),
    ];
    for (i, (label, color)) in options.iter().enumerate() {
        let is_selected = i == selected_index;
        let style = if is_selected {
            Style::default().fg(*color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(LABEL_DIM)
        };
        // Add cursor bracket around selected option
        if is_selected {
            action_spans.push(Span::styled("▸", Style::default().fg(*color).add_modifier(Modifier::BOLD)));
        } else {
            action_spans.push(Span::styled(" ", Style::default().fg(ratatui::style::Color::Reset)));
        }
        action_spans.push(Span::styled(*label, style));
        if is_selected {
            action_spans.push(Span::styled("◂", Style::default().fg(*color).add_modifier(Modifier::BOLD)));
        } else {
            action_spans.push(Span::styled(" ", Style::default().fg(ratatui::style::Color::Reset)));
        }
        // Separator between options
        if i < options.len() - 1 {
            action_spans.push(Span::styled("  ", Style::default().fg(ratatui::style::Color::Reset)));
        }
    }

    // Calculate total action width and padding
    // Total = "│ " (2) + action spans + padding + " │" (2) = inner_width + 4
    // padding = inner_width - (total span content width including │ )
    let total_span_width: usize = action_spans.iter()
        .map(|s| s.content.as_ref().width()).sum();
    let action_padding = (inner_width + 4).saturating_sub(total_span_width);
    action_spans.push(Span::styled(" ".repeat(action_padding), Style::default().fg(ratatui::style::Color::Reset)));
    action_spans.push(Span::styled(" │", Style::default().fg(APPROVAL_BORDER)));
    lines.push(Line::from(action_spans));

    // Navigation hint
    let hint = "←→ select, Enter confirm";
    let hint_padding = inner_width.saturating_sub(hint.width());
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(APPROVAL_BORDER)),
        Span::styled(hint, Style::default().fg(LABEL_DIM)),
        Span::styled(" ".repeat(hint_padding), Style::default().fg(ratatui::style::Color::Reset)),
        Span::styled(" │", Style::default().fg(APPROVAL_BORDER)),
    ]));

    // Bottom border: └────────────────────────┘
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(APPROVAL_BORDER)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(APPROVAL_BORDER)),
        Span::styled("┘", Style::default().fg(APPROVAL_BORDER)),
    ]));

    // Blank line after card
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Wrap a single line of text to fit within a maximum visual width.
///
/// Uses unicode_width to respect CJK/emoji display cell width.
fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || line.is_empty() {
        return vec![line.to_string()];
    }

    if line.width() <= max_width {
        return vec![line.to_string()];
    }

    let mut wrapped = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for ch in line.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > max_width && !current_line.is_empty() {
            // Try to break at last space
            if let Some(space_pos) = current_line.rfind(' ') {
                wrapped.push(current_line[..space_pos + 1].to_string());
                current_line = current_line[space_pos + 1..].to_string();
                current_width = current_line.width();
            } else {
                wrapped.push(current_line.clone());
                current_line.clear();
                current_width = 0;
            }
        }
        current_line.push(ch);
        current_width += ch_width;
    }
    if !current_line.is_empty() {
        wrapped.push(current_line);
    }
    wrapped
}
