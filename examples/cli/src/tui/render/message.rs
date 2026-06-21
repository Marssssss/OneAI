//! Message rendering — bubble/card style for each chat message type.
//!
//! Each message type has a distinct visual style:
//! - User: cyan bubble with border
//! - Assistant: green bubble with markdown content
//! - ToolInvocation: unified tool call+result card (Claude Code style)
//!   - Collapsed: single line "🔧 shell · echo hello → ✅"
//!   - Expanded: ── separator lines with args + result sections
//! - System: gray, no bubble
//! - Approval: yellow warning card
//! - Thinking: gray with spinner
//! - Error: red text
//! - Iteration: gray separator line

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::super::app::{ChatMessage, ChatRole};
use super::spinner::spinner_char;
use super::super::theme::*;
use super::markdown::render_markdown;
use super::diff::render_diff_lines;
use super::approval::render_approval_card_inline;

/// Render a single message as a list of Lines for the chat area.
///
/// Returns the rendered lines for this message, respecting the collapsed state,
/// available width, spinner frame, and approval selection index.
pub fn render_message_lines(msg: &ChatMessage, is_collapsed: bool, max_width: usize, spinner_frame: usize, approval_selected_index: usize) -> Vec<Line<'static>> {
    let role = &msg.role;
    let content = &msg.content;

    match role {
        ChatRole::User => render_user_message(content, max_width),
        ChatRole::Assistant => render_assistant_message(content, max_width),
        ChatRole::ToolInvocation { tool_name, args, result, .. } => {
            render_tool_invocation(tool_name, args, result, content, is_collapsed, max_width)
        }
        ChatRole::System => render_system_message(content),
        ChatRole::Iteration => render_iteration_message(content),
        ChatRole::Error => render_error_message(content),
        ChatRole::Approval => render_approval_message(content, max_width, approval_selected_index),
        ChatRole::Thinking => render_thinking_message(content, is_collapsed, max_width, spinner_frame),
    }
}

/// Render a User message as a cyan bubble (width adapts to content).
fn render_user_message(content: &str, max_width: usize) -> Vec<Line<'static>> {
    // Calculate bubble width: min(actual content visual width + padding, max_width * 0.8)
    let content_lines = wrap_content(content, max_width.saturating_sub(4));
    let max_content_line_width = content_lines.iter().map(|l| l.width()).max().unwrap_or(0);
    let bubble_width = (max_content_line_width + 14)  // title + padding
        .min((max_width as f64 * 0.8) as usize)
        .max(20)  // minimum bubble width
        .min(max_width.saturating_sub(2));

    let inner_width = bubble_width.saturating_sub(4);

    // Re-wrap content to fit the bubble width
    let content_lines = wrap_content(content, inner_width);

    let mut lines = Vec::new();

    // Top border: ┌─ 💬 User ──────┐
    // Total visual width must = inner_width + 4 (to match content/bottom lines)
    // Structure: "┌─ " (3) + title (title_visual_width) + "─"×fill_len + "┐" (1)
    // => fill_len = inner_width - title_visual_width
    let title_str = "💬 User ";
    let title_visual_width = title_str.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(USER_BORDER)),
        Span::styled(title_str, Style::default().fg(USER_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(USER_BORDER)),
        Span::styled("┐", Style::default().fg(USER_BORDER)),
    ]));

    // Content lines — pad each line to fill the inner width
    // Border overhead: "│ " (2) + content + padding + " │" (2) = inner_width + 4
    // So padding = inner_width - line_visual_width
    for line_text in content_lines {
        let line_len = line_text.width();
        let padding = inner_width.saturating_sub(line_len);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(USER_BORDER)),
            Span::styled(line_text, Style::default().fg(USER_COLOR)),
            Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(USER_BORDER)),
        ]));
    }

    // Bottom border: └──────────────┘  (total = 1 + inner_width+2 + 1 = inner_width+4)
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(USER_BORDER)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(USER_BORDER)),
        Span::styled("┘", Style::default().fg(USER_BORDER)),
    ]));

    // Blank line after bubble
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Render an Assistant message as a green bubble with markdown rendering.
fn render_assistant_message(content: &str, max_width: usize) -> Vec<Line<'static>> {
    let inner_width = max_width.saturating_sub(4);

    // Use markdown renderer for assistant messages
    let content_lines = render_markdown(content, inner_width);

    let mut lines = Vec::new();

    // Top border: ┌─ 🤖 Assistant ──┐
    // Total visual width must = inner_width + 4 (to match content/bottom lines)
    // Structure: "┌─ " (3) + emoji_span + name_span + "─"×fill_len + "┐" (1)
    // => fill_len = inner_width - emoji_visual_width - name_visual_width
    let emoji_str = "🤖 ";
    let name_str = "Assistant ";
    let title_visual_width = emoji_str.width() + name_str.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(ASSISTANT_BORDER)),
        Span::styled(emoji_str, Style::default().fg(ASSISTANT_COLOR)),
        Span::styled(name_str, Style::default().fg(ASSISTANT_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(ASSISTANT_BORDER)),
        Span::styled("┐", Style::default().fg(ASSISTANT_BORDER)),
    ]));

    // Content lines — from markdown renderer, wrapped to fit inner_width.
    // Instead of truncating overflow (which hides content), we WRAP overflow
    // onto continuation lines, each with │ borders. This ensures all content
    // is visible regardless of terminal width.
    for content_line in content_lines {
        let wrapped_line_groups = wrap_line_spans(content_line.spans, inner_width);
        for wrapped_spans in wrapped_line_groups {
            let mut line_spans = vec![
                Span::styled("│ ", Style::default().fg(ASSISTANT_BORDER)),
            ];
            let mut content_width = 0;
            for span in wrapped_spans {
                let span_width = span.content.as_ref().width();
                content_width += span_width;
                line_spans.push(span);
            }
            // Pad content to fill inner_width so the right border │ aligns
            let padding = inner_width.saturating_sub(content_width);
            if padding > 0 {
                line_spans.push(Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)));
            }
            line_spans.push(Span::styled(" │", Style::default().fg(ASSISTANT_BORDER)));
            lines.push(Line::from(line_spans));
        }
    }

    // Bottom border: └──────────────┘  (1 + inner_width+2 + 1 = inner_width+4)
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(ASSISTANT_BORDER)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(ASSISTANT_BORDER)),
        Span::styled("┘", Style::default().fg(ASSISTANT_BORDER)),
    ]));

    // Blank line after bubble
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Render a ToolInvocation as a unified card (Claude Code style — separator lines).
///
/// The block always shows the tool arguments (as readable `key = value` lines)
/// alongside the result, so the user can see *what* the tool was called with,
/// not just its output — addressing the missing-args complaint.
///
/// Collapse rule (applied to the **result** content, uniformly with thinking):
/// - Result ≤ `COLLAPSE_THRESHOLD` lines → render in full, no toggle button.
/// - Result > `COLLAPSE_THRESHOLD` lines:
///   - Collapsed (default): first `COLLAPSE_THRESHOLD` lines + "▼ expand" button.
///   - Expanded: all lines + "▲ collapse" button.
///
/// Pending (no result yet): compact single-line `🔧 tool · args ⏳` indicator.
fn render_tool_invocation(tool_name: &str, args: &str, result: &Option<(bool, String)>, _content: &str, is_collapsed: bool, max_width: usize) -> Vec<Line<'static>> {
    // ── Pending: compact single-line indicator ──────────────────────────────
    if result.is_none() {
        let args_preview = extract_tool_call_preview(tool_name, args);
        return vec![
            Line::from(vec![
                Span::styled("  🔧 ", Style::default().fg(TOOL_CALL_COLOR)),
                Span::styled(format!("{} ", tool_name), Style::default().fg(TOOL_CALL_COLOR).add_modifier(Modifier::BOLD)),
                Span::styled(args_preview, Style::default().fg(MUTED)),
                Span::styled(" ⏳", Style::default().fg(TOOL_CALL_COLOR)),
            ]),
            Line::from(Span::raw("")),
        ];
    }

    let (success, result_content) = result.as_ref().unwrap();
    let inner_width = max_width.saturating_sub(6); // 6 = "  " prefix + visual padding
    let mut lines = Vec::new();

    let (status_icon, status_color) = if *success {
        ("✅", TOOL_RESULT_SUCCESS_COLOR)
    } else {
        ("❌", TOOL_RESULT_FAILURE_COLOR)
    };
    let border_color = status_color;

    // Top separator line: ── ✅ tool_name ──────
    let title_text = format!("{} {} ", status_icon, tool_name);
    let title_visual_width = title_text.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("── ", Style::default().fg(border_color)),
        Span::styled(title_text, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(border_color)),
    ]));

    // ── Args section: readable key = value lines (always shown when args exist) ──
    let arg_lines = format_tool_args(tool_name, args, inner_width.saturating_sub(2));
    if !arg_lines.is_empty() {
        for arg_line in arg_lines {
            // arg_line looks like "  field = value" — split on the first " = "
            // to give the field name a dimmer tone than the value. Owned strings
            // are required so the spans are 'static.
            if let Some((key_part, val_part)) = arg_line.split_once(" = ") {
                lines.push(Line::from(vec![
                    Span::styled(key_part.to_string(), Style::default().fg(MUTED)),
                    Span::styled(" = ".to_string(), Style::default().fg(MUTED)),
                    Span::styled(val_part.to_string(), Style::default().fg(TEXT)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(arg_line, Style::default().fg(MUTED))));
            }
        }
    }

    // ── Result section ───────────────────────────────────────────────────────
    let result_lines = render_result_lines(result_content, *success, inner_width);

    // Result separator: ──── Result ───
    lines.push(Line::from(vec![
        Span::styled("─── Result ───", Style::default().fg(LABEL_DIM)),
    ]));

    let collapsible = result_lines.len() > COLLAPSE_THRESHOLD;

    if !collapsible {
        // Short result — show everything, no toggle button.
        lines.extend(result_lines);
    } else if is_collapsed {
        // Collapsed preview — first COLLAPSE_THRESHOLD lines + expand button.
        lines.extend(result_lines.iter().take(COLLAPSE_THRESHOLD).cloned());
        let more = result_lines.len() - COLLAPSE_THRESHOLD;
        let hint = format!("── ▼ expand ({} more lines) ", more);
        let hint_fill = inner_width.saturating_sub(hint.width());
        lines.push(Line::from(vec![
            Span::styled(hint, Style::default().fg(LABEL_DIM)),
            Span::styled("─".repeat(hint_fill), Style::default().fg(LABEL_DIM)),
        ]));
    } else {
        // Expanded — show all + collapse button.
        lines.extend(result_lines);
        let hint = "── ▲ collapse ";
        let hint_fill = inner_width.saturating_sub(hint.width());
        lines.push(Line::from(vec![
            Span::styled(hint, Style::default().fg(LABEL_DIM)),
            Span::styled("─".repeat(hint_fill), Style::default().fg(LABEL_DIM)),
        ]));
    }

    // Blank line after the block
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Render the result content of a tool invocation as a list of Lines.
///
/// Handles:
/// - Empty / placeholder results → a single "(completed successfully)" line.
/// - Diff content → coloured diff lines.
/// - Everything else → wrapped plain text.
fn render_result_lines(result_content: &str, success: bool, inner_width: usize) -> Vec<Line<'static>> {
    if result_content.is_empty() || result_content == "(completed successfully)" {
        let msg = if success { "(completed successfully)" } else { "(failed — no output)" };
        return vec![Line::from(vec![
            Span::styled("  ", Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(msg, Style::default().fg(if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR })),
        ])];
    }

    // Diff content — coloured.
    if detect_diff_content(result_content) {
        let diff_lines = render_diff_lines(result_content, inner_width.saturating_sub(2));
        return diff_lines.into_iter().map(|diff_line| {
            let mut line_spans = vec![Span::styled("  ", Style::default().fg(ratatui::style::Color::Reset))];
            line_spans.extend(diff_line.spans);
            Line::from(line_spans)
        }).collect();
    }

    // Regular output — wrap and display with the status color.
    let status_color = if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR };
    let wrapped = wrap_content(result_content, inner_width.saturating_sub(2));
    wrapped.into_iter().map(|wrapped_line| {
        Line::from(vec![
            Span::styled("  ", Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(wrapped_line, Style::default().fg(status_color)),
        ])
    }).collect()
}

/// Whether a message's collapsible content exceeds `COLLAPSE_THRESHOLD` lines at
/// the given render width. Used both by the renderer (to decide whether to draw a
/// toggle button) and by the click handler (to decide whether a click toggles).
///
/// Only ToolInvocation (with a result) and Thinking blocks are ever collapsible.
/// Pending tool calls and short content return `false` — they render in full
/// and ignore clicks.
pub fn message_is_collapsible(msg: &ChatMessage, width: usize) -> bool {
    match &msg.role {
        ChatRole::ToolInvocation { result, .. } => {
            match result {
                None => false,
                Some((_, content)) => {
                    if content.is_empty() || content == "(completed successfully)" {
                        return false;
                    }
                    let inner = width.saturating_sub(6);
                    wrap_content(content, inner).len() > COLLAPSE_THRESHOLD
                }
            }
        }
        ChatRole::Thinking => {
            if msg.content.trim().is_empty() || msg.content == "Processing your request..." {
                return false;
            }
            let inner = width.saturating_sub(4);
            wrap_content(&msg.content, inner).len() > COLLAPSE_THRESHOLD
        }
        _ => false,
    }
}

/// Extract a meaningful preview from tool call args.
///
/// Parses JSON args to show the most relevant parameter:
/// - shell: shows "command" value
/// - read_file/file_read: shows "path" value
/// - edit_file/file_edit/file_write: shows "path" value
/// - grep/search: shows "pattern" value
/// - glob: shows "pattern" value
/// - Otherwise: shows truncated JSON (up to 120 chars)
fn extract_tool_call_preview(tool_name: &str, args: &str) -> String {
    // Try to parse args as JSON to extract key fields
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(args) {
        let key_field = match tool_name {
            "shell" => "command",
            "read_file" | "file_read" => "path",
            "edit_file" | "file_edit" | "file_write" => "path",
            "grep" | "search" => "pattern",
            "glob" => "pattern",
            "list_directory" => "path",
            "notebook_edit" => "notebook_path",
            _ => "path", // default: try "path" first
        };

        // Try the tool-specific key field first
        if let Some(val) = json.get(key_field).and_then(|v| v.as_str()) {
            return truncate_str(val, 100);
        }

        // Fallback: try common fields
        for field in &["path", "command", "pattern", "query", "url"] {
            if let Some(val) = json.get(field).and_then(|v| v.as_str()) {
                return truncate_str(val, 100);
            }
        }

        // No meaningful field found — show truncated JSON
        return truncate_str(args, 120);
    }

    // Not JSON — just truncate the raw args
    truncate_str(args, 120)
}

/// Format tool arguments for display.
///
/// For JSON args, extracts key fields and displays them as readable key=value
/// lines (prefixed with two spaces) instead of raw JSON. Priority fields for the
/// given tool type are shown first; remaining fields follow. Falls back to
/// wrapped raw text for non-JSON args.
fn format_tool_args(tool_name: &str, args: &str, max_width: usize) -> Vec<String> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(args) {
        // Determine which fields to prioritize for this tool type
        let priority_fields: &[&str] = match tool_name {
            "shell" => &["command", "cwd"],
            "read_file" | "file_read" => &["path", "offset", "limit"],
            "edit_file" | "file_edit" => &["path", "old_string", "new_string"],
            "file_write" => &["path", "content"],
            "grep" | "search" => &["pattern", "path", "include"],
            "glob" => &["pattern", "path"],
            "list_directory" => &["path"],
            "notebook_edit" => &["notebook_path", "cell_id", "new_source"],
            _ => &["path", "command", "pattern", "query"],
        };

        let mut display_lines = Vec::new();

        // Show priority fields first
        for field in priority_fields {
            if let Some(val) = json.get(field) {
                let val_str = value_to_display_string(val);
                let line = format!("  {} = {}", field, truncate_str(&val_str, max_width.saturating_sub(field.len() + 6)));
                display_lines.push(line);
            }
        }

        // Show remaining fields (not already shown)
        if let serde_json::Value::Object(map) = &json {
            for (key, val) in map {
                if !priority_fields.contains(&key.as_str()) {
                    let val_str = value_to_display_string(val);
                    let line = format!("  {} = {}", key, truncate_str(&val_str, max_width.saturating_sub(key.len() + 6)));
                    display_lines.push(line);
                }
            }
        }

        if display_lines.is_empty() {
            // Fallback: show pretty-printed JSON
            let pretty = serde_json::to_string_pretty(&json)
                .unwrap_or_else(|_| args.to_string());
            return wrap_content(&pretty, max_width);
        }

        return display_lines;
    }

    // Not JSON — wrap raw text
    wrap_content(args, max_width)
}

/// Convert a JSON value to a display-friendly single-line string.
///
/// Multi-line strings (e.g. a file_write `content` arg) are collapsed to their
/// first line with a ` …(N more)` marker, so they never embed newlines that
/// would break the single-line `key = value` layout.
fn value_to_display_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => collapse_multiline(s),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(arr) => {
            // Show array as comma-separated list (truncated)
            let items: Vec<String> = arr.iter()
                .map(|v| value_to_display_string(v))
                .collect();
            truncate_str(&items.join(", "), 200)
        }
        serde_json::Value::Object(_) => {
            // For nested objects, show as compact JSON
            let compact = serde_json::to_string(val)
                .unwrap_or_default();
            truncate_str(&compact, 200)
        }
    }
}

/// Collapse a possibly multi-line string to its first line, appending a marker
/// when additional lines were dropped. Keeps arg values on a single display line.
fn collapse_multiline(s: &str) -> String {
    match s.split_once('\n') {
        None => s.to_string(),
        Some((first, rest)) => {
            let extra = rest.lines().count().max(1);
            format!("{} …({} more lines)", first.trim_end(), extra)
        }
    }
}

/// Render a System message (gray, no bubble).
fn render_system_message(content: &str) -> Vec<Line<'static>> {
    content.lines()
        .map(|line| Line::from(Span::styled(
            format!("  ⚡ {}", line),
            Style::default().fg(SYSTEM_COLOR),
        )))
        .chain(std::iter::once(Line::from(Span::raw(""))))
        .collect()
}

/// Render an Iteration separator.
fn render_iteration_message(content: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            format!("── {} ──", content),
            Style::default().fg(ratatui::style::Color::DarkGray),
        )),
        Line::from(Span::raw("")),
    ]
}

/// Render an Error message.
fn render_error_message(content: &str) -> Vec<Line<'static>> {
    content.lines()
        .map(|line| Line::from(Span::styled(
            format!("  ✗ {}", line),
            Style::default().fg(ERROR_COLOR).add_modifier(Modifier::BOLD),
        )))
        .chain(std::iter::once(Line::from(Span::raw(""))))
        .collect()
}

/// Render an Approval message (yellow warning card with adaptive width).
fn render_approval_message(content: &str, max_width: usize, approval_selected_index: usize) -> Vec<Line<'static>> {
    render_approval_card_inline(content, max_width, approval_selected_index)
}

/// Render a Thinking message (collapsible bubble with spinner).
///
/// - Placeholder / empty content (no real thinking yet): single spinner line.
/// - Content ≤ `COLLAPSE_THRESHOLD` lines: full bubble, no toggle button.
/// - Content > `COLLAPSE_THRESHOLD` lines:
///   - Collapsed: first `COLLAPSE_THRESHOLD` lines + "▼ expand" button.
///   - Expanded: all lines + "▲ collapse" button.
fn render_thinking_message(content: &str, is_collapsed: bool, max_width: usize, spinner_frame: usize) -> Vec<Line<'static>> {
    // Placeholder or empty — just the spinner, nothing to collapse.
    let is_placeholder = content.trim().is_empty() || content == "Processing your request...";
    if is_placeholder {
        let spinner = spinner_char(spinner_frame);
        return vec![
            Line::from(vec![
                Span::styled("  💭 ", Style::default().fg(THINKING_COLOR)),
                Span::styled(spinner, Style::default().fg(THINKING_COLOR)),
                Span::styled(" thinking...", THINKING_COLOR),
            ]),
            Line::from(Span::raw("")),
        ];
    }

    // Expanded: show thinking content in a bubble.
    let inner_width = max_width.saturating_sub(4);
    let mut lines = Vec::new();

    // Top border: ┌─ 💭 Thinking ──────┐
    let emoji_str = "💭 ";
    let name_str = "Thinking ";
    let title_visual_width = emoji_str.width() + name_str.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(THINKING_COLOR)),
        Span::styled(emoji_str, Style::default().fg(THINKING_COLOR)),
        Span::styled(name_str, Style::default().fg(THINKING_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(THINKING_COLOR)),
        Span::styled("┐", Style::default().fg(THINKING_COLOR)),
    ]));

    let content_lines = wrap_content(content, inner_width);
    let collapsible = content_lines.len() > COLLAPSE_THRESHOLD;

    let visible_lines: Vec<&String> = if collapsible && is_collapsed {
        content_lines.iter().take(COLLAPSE_THRESHOLD).collect()
    } else {
        content_lines.iter().collect()
    };

    for line_text in visible_lines {
        let line_width = line_text.width();
        let padding = inner_width.saturating_sub(line_width);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(THINKING_COLOR)),
            Span::styled(line_text.clone(), Style::default().fg(THINKING_COLOR)),
            Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(THINKING_COLOR)),
        ]));
    }

    // Toggle button — only when the content is long enough to collapse.
    if collapsible {
        let hint = if is_collapsed {
            format!("▼ expand ({} more lines)", content_lines.len() - COLLAPSE_THRESHOLD)
        } else {
            "▲ collapse".to_string()
        };
        let hint_width = hint.width();
        let hint_padding = inner_width.saturating_sub(hint_width);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(THINKING_COLOR)),
            Span::styled(hint, Style::default().fg(LABEL_DIM)),
            Span::styled(" ".repeat(hint_padding), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(THINKING_COLOR)),
        ]));
    }

    // Bottom border: └─────────────────────┘
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(THINKING_COLOR)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(THINKING_COLOR)),
        Span::styled("┘", Style::default().fg(THINKING_COLOR)),
    ]));

    // Blank line after bubble
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Truncate a span's content to a maximum visual width, appending "…" if truncated.
///
/// This prevents content lines from exceeding the bubble's inner_width,
/// which would push the right border │ off-screen.
#[allow(dead_code)]
fn truncate_span_to_width(content: &str, max_visual_width: usize) -> String {
    if max_visual_width <= 1 {
        return if max_visual_width == 1 { "…".to_string() } else { String::new() };
    }
    if content.width() <= max_visual_width {
        return content.to_string();
    }
    // Find the byte position where visual width reaches max_visual_width - 1
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

/// Wrap content text to fit within a given visual width.
///
/// Uses `unicode_width` for proper CJK/emoji handling — each character's
/// display cell width is respected, not just byte length.
fn wrap_content(content: &str, max_width: usize) -> Vec<String> {
    if max_width <= 0 {
        return vec![content.to_string()];
    }

    content.lines()
        .flat_map(|line| {
            let visual_width = line.width();
            if visual_width <= max_width {
                vec![line.to_string()]
            } else {
                // Wrap by visual cell width, breaking at spaces where possible
                let mut wrapped = Vec::new();
                let mut current_line = String::new();
                let mut current_width = 0;

                for ch in line.chars() {
                    let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if current_width + ch_width > max_width && !current_line.is_empty() {
                        // Try to break at last space in the current line
                        if let Some(space_pos) = current_line.rfind(' ') {
                            wrapped.push(current_line[..space_pos + 1].to_string());
                            current_line = current_line[space_pos + 1..].to_string();
                            current_width = current_line.width();
                        } else {
                            // No space found — hard break
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
                if wrapped.is_empty() {
                    wrapped.push(line.to_string());
                }
                wrapped
            }
        })
        .collect()
}

/// Truncate a string to a maximum visual width with "..." suffix.
///
/// Uses unicode_width to respect CJK/emoji display width.
fn truncate_str(s: &str, max_visual_width: usize) -> String {
    if max_visual_width <= 3 {
        return "...".to_string();
    }

    let mut total_width = 0;
    let mut byte_pos = 0;

    for (i, ch) in s.char_indices() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > max_visual_width - 3 {
            break;
        }
        total_width += ch_width;
        byte_pos = i + ch.len_utf8();
    }

    if byte_pos >= s.len() && total_width <= max_visual_width {
        s.to_string()
    } else {
        format!("{}...", &s[..byte_pos])
    }
}

/// Detect if content is a diff output.
///
/// Checks for:
/// - Unified diff headers (+++ / --- / @@)
/// - Multiple consecutive +/- lines (at least 3 diff lines)
/// - Diff summary format (e.g., "X additions, Y deletions")
fn detect_diff_content(content: &str) -> bool {
    let lines = content.lines().collect::<Vec<_>>();

    // Check for unified diff headers
    if lines.iter().any(|l| l.starts_with("+++ ") || l.starts_with("--- ")) {
        return true;
    }
    if lines.iter().any(|l| l.starts_with("@@")) {
        return true;
    }

    // Check for diff summary patterns
    if content.contains("additions") && content.contains("deletions") {
        return true;
    }

    // Check for multiple consecutive +/- lines (at least 3)
    let diff_line_count = lines.iter()
        .filter(|l| l.starts_with('+') && !l.starts_with("++") || l.starts_with('-') && !l.starts_with("--"))
        .count();
    diff_line_count >= 3
}

/// Wrap a line's spans into multiple groups, each group's total visual width ≤ max_width.
///
/// This replaces truncation with proper wrapping: when a Line from the markdown
/// renderer exceeds the available width, overflow content is split onto continuation
/// groups instead of being discarded. Each group will be rendered as a separate
/// bordered line in the message bubble.
///
/// The wrapping algorithm:
/// 1. Walk through spans, accumulating visual width
/// 2. When the total width would exceed max_width:
///    a. Try to break at the last space within the current span (word-wrap)
///    b. If no space found, hard-break at the character boundary
/// 3. Overflow content starts a new group, continuing until all content is distributed
///
/// Returns a list of Vec<Span> groups, each group fitting within max_width.
fn wrap_line_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width <= 0 {
        return vec![spans];
    }

    // Check if all spans fit within max_width — no wrapping needed
    let total_width: usize = spans.iter().map(|s| s.content.as_ref().width()).sum();
    if total_width <= max_width {
        return vec![spans];
    }

    let mut groups: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_group: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0;

    for span in spans {
        let span_width = span.content.as_ref().width();

        if current_width + span_width <= max_width {
            // Span fits entirely in the current group
            current_width += span_width;
            current_group.push(span);
            continue;
        }

        // Span would exceed max_width — need to split it
        let remaining = max_width.saturating_sub(current_width);

        if remaining > 0 {
            // Fill the remaining space in the current group with part of this span
            let (fit_part, overflow_part) = split_span_at_width(&span, remaining);
            if let Some(fit) = fit_part {
                current_width += fit.content.as_ref().width();
                current_group.push(fit);
            }

            // Finalize current group and start a new one
            if !current_group.is_empty() {
                groups.push(current_group);
                current_group = Vec::new();
                current_width = 0;
            }

            // Process the overflow part — it may itself need to be split across multiple groups
            if let Some(overflow) = overflow_part {
                process_overflow_span(overflow, max_width, &mut groups, &mut current_group, &mut current_width);
            }
        } else {
            // No remaining space — finalize current group (even if empty) and start fresh
            if !current_group.is_empty() {
                groups.push(current_group);
                current_group = Vec::new();
                current_width = 0;
            }

            // Process this entire span as overflow
            process_overflow_span(span, max_width, &mut groups, &mut current_group, &mut current_width);
        }
    }

    // Push any remaining content in the current group
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    // Ensure we always have at least one group (even for empty input)
    if groups.is_empty() {
        groups.push(Vec::new());
    }

    groups
}

/// Process a span that may exceed max_width, splitting it across multiple groups.
///
/// Handles spans that are wider than max_width by repeatedly splitting
/// at width boundaries until all content is distributed into groups.
fn process_overflow_span(
    span: Span<'static>,
    max_width: usize,
    groups: &mut Vec<Vec<Span<'static>>>,
    current_group: &mut Vec<Span<'static>>,
    current_width: &mut usize,
) {
    let span_width = span.content.as_ref().width();

    if span_width <= max_width {
        // Span fits in a new group
        *current_width = span_width;
        current_group.push(span);
        return;
    }

    // Span exceeds max_width — split it repeatedly
    let mut remaining_span = span;
    while remaining_span.content.as_ref().width() > 0 {
        let remaining_width = remaining_span.content.as_ref().width();

        if remaining_width <= max_width {
            // Final piece fits in the current group
            *current_width = remaining_width;
            current_group.push(remaining_span);
            break;
        }

        // Split at max_width boundary
        let (fit_part, overflow_part) = split_span_at_width(&remaining_span, max_width);

        if let Some(fit) = fit_part {
            // Finalize current group with the fit part
            let mut complete_group = current_group.clone();
            complete_group.push(fit);
            groups.push(complete_group);
            current_group.clear();
            *current_width = 0;
        } else {
            // Can't fit anything — skip this span content (shouldn't happen with max_width > 0)
            break;
        }

        if let Some(overflow) = overflow_part {
            remaining_span = overflow;
        } else {
            break;
        }
    }
}

/// Split a Span at a visual width boundary, returning (fit_part, overflow_part).
///
/// Tries to break at a space character within the span for word-wrap.
/// If no suitable space is found, performs a hard character-boundary break.
fn split_span_at_width(span: &Span<'static>, max_visual_width: usize) -> (Option<Span<'static>>, Option<Span<'static>>) {
    if max_visual_width <= 0 {
        return (None, Some(span.clone()));
    }

    let content = span.content.as_ref();
    let content_width = content.width();

    if content_width <= max_visual_width {
        // Entire span fits — no split needed
        return (Some(span.clone()), None);
    }

    // Try word-wrap: find the last space character that keeps the visual width ≤ max_visual_width
    let mut best_space_byte_pos = None;
    let mut total_width = 0;
    for (byte_pos, ch) in content.char_indices() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > max_visual_width {
            break;
        }
        total_width += ch_width;
        if ch == ' ' {
            best_space_byte_pos = Some(byte_pos + ch.len_utf8()); // position after the space
        }
    }

    // If we found a space within the width limit, break after it (word-wrap)
    // But only if the result isn't just whitespace (i.e., the part before the space has content)
    if let Some(space_pos) = best_space_byte_pos {
        let fit_str = &content[..space_pos];
        let overflow_str = &content[space_pos..];
        // Only use word-wrap if the fit part is non-empty (not just spaces)
        if !fit_str.trim().is_empty() {
            return (
                Some(Span::styled(fit_str.to_string(), span.style)),
                Some(Span::styled(overflow_str.to_string(), span.style)),
            );
        }
    }

    // No suitable space found — hard break at character boundary
    let mut total_width = 0;
    let mut byte_pos = 0;
    for (i, ch) in content.char_indices() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > max_visual_width {
            break;
        }
        total_width += ch_width;
        byte_pos = i + ch.len_utf8();
    }

    if byte_pos == 0 {
        // Can't fit even one character — put entire span in overflow
        return (None, Some(span.clone()));
    }

    let fit_str = &content[..byte_pos];
    let overflow_str = &content[byte_pos..];

    (
        Some(Span::styled(fit_str.to_string(), span.style)),
        Some(Span::styled(overflow_str.to_string(), span.style)),
    )
}

/// Format tool output content for display.
///
/// If the content is JSON, pretty-print it with syntax highlighting.
/// Otherwise, wrap plain text with the appropriate color.
/// Empty/whitespace content shows a success indicator instead of blank output.
#[allow(dead_code)]
fn format_tool_output(content: &str, max_width: usize) -> Vec<Line<'static>> {
    // Handle empty or whitespace-only content (e.g., mkdir returns empty string)
    if content.trim().is_empty() {
        return vec![Line::from(Span::styled(
            "(completed successfully — no output)",
            Style::default().fg(TOOL_RESULT_SUCCESS_COLOR),
        ))];
    }

    // Try to parse as JSON for pretty-printing
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(content) {
        let pretty = serde_json::to_string_pretty(&json_value)
            .unwrap_or_else(|_| content.to_string());
        return wrap_content(&pretty, max_width)
            .into_iter()
            .map(|t| Line::from(Span::styled(t, Style::default().fg(ratatui::style::Color::Rgb(184, 180, 160))))
            )
            .collect();
    }

    // Plain text — wrap with appropriate styling
    wrap_content(content, max_width)
        .into_iter()
        .map(|t| Line::from(Span::styled(t, Style::default().fg(ratatui::style::Color::Green))))
        .collect()
}
