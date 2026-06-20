//! Diff rendering — visualizes +/- line changes with color coding.
//!
//! Renders diff output with:
//! - Green/red background for added/deleted lines
//! - Line number annotations
//! - Adaptive width based on available space — lines are truncated to max_width
//!   so content never exceeds the bubble viewport width
//! - Folded diff summaries for collapsed view
//! - Proper handling of diff headers (+++ ---) and hunk markers (@@)

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::super::theme::*;

/// Render a diff section with +/- line markers.
///
/// This renderer handles unified diff format:
/// - Lines starting with `+++` or `---` are file headers (cyan)
/// - Lines starting with `@@` are hunk markers (magenta)
/// - Lines starting with `+` (not `++`) are additions (green)
/// - Lines starting with `-` (not `--`) are deletions (red)
/// - Other lines are context (gray)
///
/// All lines are truncated to `max_width` visual cells to prevent content
/// from exceeding the bubble viewport width (which causes rendering artifacts).
pub fn render_diff_lines(content: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut old_line_no: usize = 0;
    let mut new_line_no: usize = 0;

    for raw_line in content.lines() {
        // Parse hunk header to get line numbers
        if raw_line.starts_with("@@") {
            // Try to extract line numbers from @@ -old_start,count +new_start,count @@
            if let Some(nums) = parse_hunk_header(raw_line) {
                old_line_no = nums.old_start;
                new_line_no = nums.new_start;
            }
            lines.push(render_hunk_header(raw_line, max_width));
            continue;
        }

        // File headers (+++ ---)
        if raw_line.starts_with("+++") || raw_line.starts_with("---") {
            lines.push(render_file_header(raw_line, max_width));
            continue;
        }

        // Added line — green with line number
        if raw_line.starts_with('+') && !raw_line.starts_with("++") {
            new_line_no += 1;
            lines.push(render_added_line(raw_line, new_line_no, max_width));
            continue;
        }

        // Deleted line — red with line number
        if raw_line.starts_with('-') && !raw_line.starts_with("--") {
            old_line_no += 1;
            lines.push(render_deleted_line(raw_line, old_line_no, max_width));
            continue;
        }

        // Context line — gray with line numbers
        old_line_no += 1;
        new_line_no += 1;
        lines.push(render_context_line(raw_line, old_line_no, new_line_no, max_width));
    }

    lines
}

/// Render a collapsed diff summary — shows just the stats line.
///
/// Format: `diff: +N additions, -N deletions (X lines)`
#[allow(dead_code)]
pub fn render_diff_summary(content: &str) -> Vec<Line<'static>> {
    let stats = compute_diff_stats(content);
    vec![
        Line::from(vec![
            Span::styled("📊 ", Style::default().fg(TOOL_RESULT_SUCCESS_COLOR)),
            Span::styled(
                format!("diff: +{} additions, -{} deletions ({})", stats.additions, stats.deletions, stats.total_lines),
                Style::default().fg(TOOL_RESULT_SUCCESS_COLOR),
            ),
        ]),
        Line::from(Span::raw("")),
    ]
}

// ─── Individual Line Renderers ──────────────────────────────────────────────

/// Truncate a span's content to fit within a maximum visual width.
/// Returns a new Span with content truncated and "…" appended if it exceeds max_width.
fn truncate_span_content(content: &str, max_visual_width: usize) -> String {
    if max_visual_width <= 1 {
        return if max_visual_width == 1 { "…".to_string() } else { String::new() };
    }
    if content.width() <= max_visual_width {
        return content.to_string();
    }
    // Need to truncate — find the byte position where visual width reaches max_visual_width - 1
    // (leaving 1 cell for the "…" suffix)
    let mut total_width = 0;
    let mut byte_pos = 0;
    for (i, ch) in content.char_indices() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > max_visual_width - 1 {
            break;
        }
        total_width += ch_width;
        byte_pos = i + ch.len_utf8();
    }
    format!("{}…", &content[..byte_pos])
}

fn render_hunk_header(line: &str, max_width: usize) -> Line<'static> {
    let truncated = truncate_span_content(line, max_width);
    Line::from(Span::styled(
        truncated,
        Style::default().fg(ratatui::style::Color::Magenta).add_modifier(Modifier::BOLD),
    ))
}

fn render_file_header(line: &str, max_width: usize) -> Line<'static> {
    let truncated = truncate_span_content(line, max_width);
    Line::from(Span::styled(
        truncated,
        Style::default().fg(ratatui::style::Color::Cyan),
    ))
}

fn render_added_line(line: &str, line_no: usize, max_width: usize) -> Line<'static> {
    let line_no_width = format!("{:>4} ", line_no).width(); // "   1 " = 5 cells
    let prefix_width = 1; // "+" character
    let content_max_width = max_width.saturating_sub(line_no_width + prefix_width);
    let content = if line.len() > 1 { &line[1..] } else { "" };
    let truncated_content = truncate_span_content(content, content_max_width);
    let line_no_str = format!("{:>4} ", line_no);
    Line::from(vec![
        Span::styled(line_no_str, Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled("+", Style::default().fg(ratatui::style::Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(truncated_content, Style::default().fg(ratatui::style::Color::Green).bg(DIFF_ADDED_BG)),
    ])
}

fn render_deleted_line(line: &str, line_no: usize, max_width: usize) -> Line<'static> {
    let line_no_width = format!("{:>4} ", line_no).width();
    let prefix_width = 1; // "-" character
    let content_max_width = max_width.saturating_sub(line_no_width + prefix_width);
    let content = if line.len() > 1 { &line[1..] } else { "" };
    let truncated_content = truncate_span_content(content, content_max_width);
    let line_no_str = format!("{:>4} ", line_no);
    Line::from(vec![
        Span::styled(line_no_str, Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled("-", Style::default().fg(ratatui::style::Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(truncated_content, Style::default().fg(ratatui::style::Color::Red).bg(DIFF_DELETED_BG)),
    ])
}

fn render_context_line(line: &str, old_no: usize, new_no: usize, max_width: usize) -> Line<'static> {
    let line_no_width = format!("{:>4}/{:>4} ", old_no, new_no).width();
    let prefix_width = 1; // space character
    let content_max_width = max_width.saturating_sub(line_no_width + prefix_width);
    let line_no_str = format!("{:>4}/{:>4} ", old_no, new_no);
    let truncated_content = truncate_span_content(line, content_max_width);
    Line::from(vec![
        Span::styled(line_no_str, Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(" ", Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(truncated_content, Style::default().fg(ratatui::style::Color::DarkGray)),
    ])
}

// ─── Parsing Helpers ────────────────────────────────────────────────────────

/// Hunk header line numbers extracted from `@@ -old_start,count +new_start,count @@`.
struct HunkNumbers {
    old_start: usize,
    new_start: usize,
}

fn parse_hunk_header(line: &str) -> Option<HunkNumbers> {
    // Format: @@ -old_start[,count] +new_start[,count] @@
    // Example: @@ -1,3 +1,4 @@
    let line = line.trim();
    if !line.starts_with("@@") {
        return None;
    }

    // Find the -old and +new parts
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    let old_part = parts.get(1)?; // "-1,3" or "-1"
    let new_part = parts.get(2)?; // "+1,4" or "+1"

    let old_start = old_part
        .strip_prefix('-')
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse::<usize>().ok())?;

    let new_start = new_part
        .strip_prefix('+')
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse::<usize>().ok())?;

    Some(HunkNumbers { old_start, new_start })
}

/// Diff statistics (additions, deletions, total lines).
struct DiffStats {
    additions: usize,
    deletions: usize,
    total_lines: usize,
}

fn compute_diff_stats(content: &str) -> DiffStats {
    let mut additions = 0;
    let mut deletions = 0;
    let total_lines = content.lines().count();

    for line in content.lines() {
        if line.starts_with('+') && !line.starts_with("++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("--") {
            deletions += 1;
        }
    }

    DiffStats { additions, deletions, total_lines }
}
