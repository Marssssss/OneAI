//! Message rendering — bubble/card style for each chat message type.
//!
//! Each message type has a distinct visual style:
//! - User: cyan bubble with border
//! - Assistant: green bubble with markdown content
//! - ToolCall: magenta card (collapsible)
//! - ToolResult: blue/green or red card (collapsible for long content)
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
        ChatRole::ToolCall { tool_name, args, .. } => {
            if is_collapsed {
                render_tool_call_collapsed(tool_name, args)
            } else {
                render_tool_call_expanded(tool_name, args, content, max_width)
            }
        }
        ChatRole::ToolResult { success, tool_name, .. } => {
            let is_file_op = super::super::app::is_file_operation_tool(tool_name);
            if is_collapsed {
                if is_file_op {
                    render_file_result_collapsed(content, *success, tool_name)
                } else {
                    render_tool_result_collapsed(content, *success)
                }
            } else {
                if is_file_op {
                    render_file_result_expanded(content, *success, tool_name, max_width)
                } else {
                    render_tool_result_expanded(content, *success, max_width)
                }
            }
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

    // Content lines — from markdown renderer
    // Total visual width: "│ " (2) + content (content_width) + padding + " │" (2) = inner_width + 4
    // So padding = inner_width - content_width. If content_width > inner_width, padding = 0
    // but the │ would be pushed off-screen. To prevent this, we truncate spans that
    // would exceed inner_width, ensuring the │ is always visible.
    for content_line in content_lines {
        let mut line_spans = vec![
            Span::styled("│ ", Style::default().fg(ASSISTANT_BORDER)),
        ];
        // Reconstruct spans from the markdown line, truncating if total exceeds inner_width
        let mut content_width = 0;
        for span in content_line.spans {
            let span_width = span.content.as_ref().width();
            if content_width + span_width <= inner_width {
                // Span fits within inner_width — include it entirely
                content_width += span_width;
                line_spans.push(span);
            } else {
                // Span would exceed inner_width — truncate it to fit the remaining space
                let remaining = inner_width.saturating_sub(content_width);
                if remaining > 0 {
                    let truncated = truncate_span_to_width(&span.content, remaining);
                    let truncated_width = truncated.width();
                    content_width += truncated_width;
                    line_spans.push(Span::styled(truncated, span.style));
                }
                // No more room — stop adding spans
                break;
            }
        }
        // Pad content to fill inner_width so the right border │ aligns
        let padding = inner_width.saturating_sub(content_width);
        if padding > 0 {
            line_spans.push(Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)));
        }
        line_spans.push(Span::styled(" │", Style::default().fg(ASSISTANT_BORDER)));
        lines.push(Line::from(line_spans));
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

/// Render a collapsed ToolCall card (just tool name + args summary).
///
/// Shows a meaningful preview of the tool arguments:
/// - For shell commands: shows the actual command
/// - For file operations: shows the file path
/// - For other tools: shows truncated JSON args (up to 120 chars)
fn render_tool_call_collapsed(tool_name: &str, args: &str) -> Vec<Line<'static>> {
    // Try to extract a meaningful preview from JSON args
    let args_preview = extract_tool_call_preview(tool_name, args);
    vec![
        Line::from(vec![
            Span::styled("┌─ ", Style::default().fg(TOOL_CALL_BORDER)),
            Span::styled("🔧 ", Style::default().fg(TOOL_CALL_COLOR)),
            Span::styled(format!("{} ", tool_name), Style::default().fg(TOOL_CALL_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(args_preview, Style::default().fg(ratatui::style::Color::DarkGray)),
            Span::styled(" ── ▸┐", Style::default().fg(TOOL_CALL_BORDER)),
        ]),
        Line::from(Span::raw("")),
    ]
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

/// Render an expanded ToolCall card.
///
/// Shows the full tool name, arguments (with JSON formatting if applicable),
/// and result content. Arguments are displayed with key-value highlighting
/// for better readability.
fn render_tool_call_expanded(tool_name: &str, args: &str, content: &str, max_width: usize) -> Vec<Line<'static>> {
    let inner_width = max_width.saturating_sub(4);

    let mut lines = Vec::new();

    // Top border with tool name
    let title_text = format!("🔧 {} ", tool_name);
    let title_visual_width = title_text.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(TOOL_CALL_BORDER)),
        Span::styled(title_text, Style::default().fg(TOOL_CALL_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(TOOL_CALL_BORDER)),
        Span::styled("┐", Style::default().fg(TOOL_CALL_BORDER)),
    ]));

    // Args section — try to show as formatted key-value pairs for better readability
    let args_display_lines = format_tool_args(tool_name, args, inner_width);
    // Args label line
    let args_label_width = "Args:".width();
    let args_label_pad = inner_width.saturating_sub(args_label_width);
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(TOOL_CALL_BORDER)),
        Span::styled("Args:", Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(" ".repeat(args_label_pad), Style::default().fg(ratatui::style::Color::Reset)),
        Span::styled(" │", Style::default().fg(TOOL_CALL_BORDER)),
    ]));
    for arg_line in args_display_lines {
        let line_width = arg_line.width();
        let padding = inner_width.saturating_sub(line_width);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(TOOL_CALL_BORDER)),
            Span::styled(arg_line, Style::default().fg(TOOL_CALL_COLOR)),
            Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(TOOL_CALL_BORDER)),
        ]));
    }

    // Content section (result)
    if !content.is_empty() {
        let result_label = "─── Result ───";
        let result_label_width = result_label.width();
        let result_label_pad = inner_width.saturating_sub(result_label_width);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(TOOL_CALL_BORDER)),
            Span::styled(result_label, Style::default().fg(ratatui::style::Color::DarkGray)),
            Span::styled(" ".repeat(result_label_pad), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(TOOL_CALL_BORDER)),
        ]));
        let result_lines = wrap_content(content, inner_width);
        for res_line in result_lines {
            let line_width = res_line.width();
            let padding = inner_width.saturating_sub(line_width);
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(TOOL_CALL_BORDER)),
                Span::styled(res_line, Style::default().fg(TOOL_CALL_COLOR)),
                Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)),
                Span::styled(" │", Style::default().fg(TOOL_CALL_BORDER)),
            ]));
        }
    }

    // Bottom border
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(TOOL_CALL_BORDER)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(TOOL_CALL_BORDER)),
        Span::styled("┘", Style::default().fg(TOOL_CALL_BORDER)),
    ]));
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Format tool arguments for display.
///
/// For JSON args, extracts key fields and displays them as readable key=value lines
/// instead of raw JSON. This makes it much easier to see what a tool is doing.
/// Falls back to wrapped raw text for non-JSON args.
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

/// Convert a JSON value to a display-friendly string.
fn value_to_display_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
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

/// Render a collapsed ToolResult (success/failure indicator + preview).
fn render_tool_result_collapsed(content: &str, success: bool) -> Vec<Line<'static>> {
    let color = if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR };
    let border_color = if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR };
    let icon = if success { "✅" } else { "❌" };
    let preview = truncate_str(content, 80);
    vec![
        Line::from(vec![
            Span::styled("┌─ ", Style::default().fg(border_color)),
            Span::styled(format!("{} ", icon), Style::default().fg(color)),
            Span::styled(preview, Style::default().fg(color)),
            Span::styled(" ── ▸┐", Style::default().fg(border_color)),
        ]),
        Line::from(Span::raw("")),
    ]
}

/// Render an expanded ToolResult.
fn render_tool_result_expanded(content: &str, success: bool, max_width: usize) -> Vec<Line<'static>> {
    let inner_width = max_width.saturating_sub(4);
    let color = if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR };
    let border_color = color;
    let icon = if success { "✅ Result" } else { "❌ Error" };

    // Detect if content looks like a diff output
    let is_diff = detect_diff_content(content);

    let content_rendered = if is_diff {
        // Use diff renderer for diff-like content
        render_diff_lines(content, inner_width)
    } else {
        // Try to detect JSON output and format it nicely
        let formatted = format_tool_output(content, inner_width);
        formatted
    };

    let mut lines = Vec::new();

    // Top border
    // Structure: "┌─ " (3) + icon_span + "─"×fill_len + "┐" (1)
    // Total = inner_width + 4, so fill_len = inner_width - icon_span_visual_width
    let icon_str = format!("{} ", icon);
    let icon_visual_width = icon_str.width();
    let fill_len = inner_width.saturating_sub(icon_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(border_color)),
        Span::styled(icon_str, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(border_color)),
        Span::styled("┐", Style::default().fg(border_color)),
    ]));

    // Content lines — from rendered content (diff or plain)
    // Total = "│ " (2) + content + padding + " │" (2) = inner_width + 4
    // So padding = inner_width - content_width. Truncate spans that would exceed
    // inner_width to prevent │ from being pushed off-screen (causing rendering artifacts).
    for content_line in content_rendered {
        let mut line_spans = vec![
            Span::styled("│ ", Style::default().fg(border_color)),
        ];
        let mut content_width = 0;
        for span in content_line.spans {
            let span_width = span.content.as_ref().width();
            if content_width + span_width <= inner_width {
                content_width += span_width;
                line_spans.push(span);
            } else {
                // Truncate span to fit remaining space
                let remaining = inner_width.saturating_sub(content_width);
                if remaining > 0 {
                    let truncated = truncate_span_to_width(&span.content, remaining);
                    let truncated_width = truncated.width();
                    content_width += truncated_width;
                    line_spans.push(Span::styled(truncated, span.style));
                }
                break;
            }
        }
        let padding = inner_width.saturating_sub(content_width);
        if padding > 0 {
            line_spans.push(Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)));
        }
        line_spans.push(Span::styled(" │", Style::default().fg(border_color)));
        lines.push(Line::from(line_spans));
    }

    // Bottom border: └──────────────┘  (1 + inner_width+2 + 1 = inner_width+4)
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(border_color)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(border_color)),
        Span::styled("┘", Style::default().fg(border_color)),
    ]));
    lines.push(Line::from(Span::raw("")));

    lines
}

/// Render a collapsed file operation result.
///
/// Shows a 3-line preview of the file content with a ▸ expand indicator.
/// For write/edit operations with empty output, shows a success message.
fn render_file_result_collapsed(content: &str, success: bool, tool_name: &str) -> Vec<Line<'static>> {
    let color = if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR };
    let border_color = color;

    if content.trim().is_empty() {
        // Empty content (e.g., write/edit succeeded) — show success indicator
        let icon = if success { "✅" } else { "❌" };
        let label = format!("{} completed", tool_name);
        vec![
            Line::from(vec![
                Span::styled("┌─ ", Style::default().fg(border_color)),
                Span::styled(format!("{} ", icon), Style::default().fg(color)),
                Span::styled(label, Style::default().fg(color)),
                Span::styled(" ── ▸┐", Style::default().fg(border_color)),
            ]),
            Line::from(Span::raw("")),
        ]
    } else {
        // Show 3-line preview with expand indicator
        let all_lines: Vec<&str> = content.lines().collect();
        let preview_lines: Vec<&str> = all_lines.iter().take(3).copied().collect();
        let total_lines = all_lines.len();
        let more_count = total_lines.saturating_sub(3);

        let icon = if success { "✅" } else { "❌" };

        if total_lines <= 3 {
            // Content fits in 3 lines — show all with expand hint
            let preview_text = preview_lines.join(" │ ");
            vec![
                Line::from(vec![
                    Span::styled("┌─ ", Style::default().fg(border_color)),
                    Span::styled(format!("{} ", icon), Style::default().fg(color)),
                    Span::styled(truncate_str(&preview_text, 100), Style::default().fg(color)),
                    Span::styled(" ── ▸┐", Style::default().fg(border_color)),
                ]),
                Line::from(Span::raw("")),
            ]
        } else {
            // More lines available — show count
            vec![
                Line::from(vec![
                    Span::styled("┌─ ", Style::default().fg(border_color)),
                    Span::styled(format!("{} ", icon), Style::default().fg(color)),
                    Span::styled(truncate_str(&preview_lines.join(" │ "), 80), Style::default().fg(color)),
                    Span::styled(format!(" ▸({} more lines)", more_count), Style::default().fg(ratatui::style::Color::DarkGray)),
                    Span::styled("┐", Style::default().fg(border_color)),
                ]),
                Line::from(Span::raw("")),
            ]
        }
    }
}

/// Render an expanded file operation result.
///
/// Shows the full file content in a card with line numbers.
/// Content is displayed as code-like text with │ separators.
fn render_file_result_expanded(content: &str, success: bool, tool_name: &str, max_width: usize) -> Vec<Line<'static>> {
    let inner_width = max_width.saturating_sub(4);
    let color = if success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR };
    let border_color = color;

    let mut lines = Vec::new();

    // Top border with tool name and result indicator
    let icon = if success { "✅" } else { "❌" };
    let title_text = format!("{} {} ", icon, tool_name);
    let title_visual_width = title_text.width();
    let fill_len = inner_width.saturating_sub(title_visual_width);
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(border_color)),
        Span::styled(title_text, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill_len), Style::default().fg(border_color)),
        Span::styled("┐", Style::default().fg(border_color)),
    ]));

    if content.trim().is_empty() {
        // No content — show success indicator
        let msg = "(completed successfully)";
        let msg_width = msg.width();
        let msg_pad = inner_width.saturating_sub(msg_width);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(border_color)),
            Span::styled(msg, Style::default().fg(color)),
            Span::styled(" ".repeat(msg_pad), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(border_color)),
        ]));
    } else {
        // Show file content with line numbers
        let content_lines: Vec<&str> = content.lines().collect();
        let total_lines = content_lines.len();

        // Calculate line number width (e.g., " 3" for up to 9 lines, "10" for up to 99)
        let line_num_width = if total_lines < 10 { 2 }
            else if total_lines < 100 { 3 }
            else { 4 };

        // Content width = inner_width - line_num_width - 2 (for " │" separator)
        let content_width = inner_width.saturating_sub(line_num_width + 2);

        for (i, line_text) in content_lines.iter().enumerate() {
            let line_num = format!("{:>width$}", i + 1, width = line_num_width);
            let wrapped = wrap_content(line_text, content_width);

            for (j, wrapped_line) in wrapped.iter().enumerate() {
                let mut line_spans = vec![
                    Span::styled("│ ", Style::default().fg(border_color)),
                ];
                // Show line number only on first wrapped line
                if j == 0 {
                    line_spans.push(Span::styled(line_num.clone(), Style::default().fg(ratatui::style::Color::DarkGray)));
                    line_spans.push(Span::styled(" │ ", Style::default().fg(ratatui::style::Color::DarkGray)));
                } else {
                    line_spans.push(Span::styled(" ".repeat(line_num_width), Style::default().fg(ratatui::style::Color::Reset)));
                    line_spans.push(Span::styled(" │ ", Style::default().fg(ratatui::style::Color::DarkGray)));
                }
                let w_line_width = wrapped_line.width();
                line_spans.push(Span::styled(wrapped_line.clone(), Style::default().fg(color)));
                let padding = inner_width.saturating_sub(line_num_width + 2 + w_line_width + 2);
                if padding > 0 {
                    line_spans.push(Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)));
                }
                line_spans.push(Span::styled(" │", Style::default().fg(border_color)));
                lines.push(Line::from(line_spans));
            }
        }
    }

    // Bottom border with collapse hint
    let collapse_hint = "── ▼ collapse ──";
    let hint_width = collapse_hint.width();
    let hint_pad = inner_width.saturating_sub(hint_width);
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(border_color)),
        Span::styled(collapse_hint, Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(" ".repeat(hint_pad), Style::default().fg(ratatui::style::Color::Reset)),
        Span::styled(" │", Style::default().fg(border_color)),
    ]));

    // Bottom border
    lines.push(Line::from(vec![
        Span::styled("└", Style::default().fg(border_color)),
        Span::styled("─".repeat(inner_width + 2), Style::default().fg(border_color)),
        Span::styled("┘", Style::default().fg(border_color)),
    ]));
    lines.push(Line::from(Span::raw("")));

    lines
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

/// Render a Thinking message (spinner + gray text).
/// Render a Thinking message (collapsible bubble with spinner).
///
/// Collapsed (default): single spinner line "💭 thinking..."
/// Expanded: bubble showing thinking content with "▼ more" hint at bottom.
fn render_thinking_message(content: &str, is_collapsed: bool, max_width: usize, spinner_frame: usize) -> Vec<Line<'static>> {
    let spinner = spinner_char(spinner_frame);

    if is_collapsed {
        // Collapsed: just show spinner + "thinking..."
        vec![
            Line::from(vec![
                Span::styled("  💭 ", Style::default().fg(THINKING_COLOR)),
                Span::styled(spinner, Style::default().fg(THINKING_COLOR)),
                Span::styled(" thinking...", THINKING_COLOR),
            ]),
            Line::from(Span::raw("")),
        ]
    } else {
        // Expanded: show thinking content in a bubble
        let inner_width = max_width.saturating_sub(4);
        let mut lines = Vec::new();

        // Top border: ┌─ 💭 Thinking ──────┐
        // Structure: "┌─ " (3) + emoji + name + "─"×fill_len + "┐" (1)
        // Total = inner_width + 4, so fill_len = inner_width - emoji_width - name_width
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

        // Content lines — show thinking content (limited to available width)
        // Total = "│ " (2) + line + padding + " │" (2) = inner_width + 4
        // So padding = inner_width - line_width
        let content_lines = wrap_content(content, inner_width);
        for line_text in content_lines {
            let line_width = line_text.width();
            let padding = inner_width.saturating_sub(line_width);
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(THINKING_COLOR)),
                Span::styled(line_text, Style::default().fg(THINKING_COLOR)),
                Span::styled(" ".repeat(padding), Style::default().fg(ratatui::style::Color::Reset)),
                Span::styled(" │", Style::default().fg(THINKING_COLOR)),
            ]));
        }

        // Bottom hint: "▼ more / collapse"
        // Same padding formula: inner_width - hint_width
        let hint = "▼ more lines — press to collapse";
        let hint_width = hint.width();
        let hint_padding = inner_width.saturating_sub(hint_width);
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(THINKING_COLOR)),
            Span::styled(hint, Style::default().fg(ratatui::style::Color::DarkGray)),
            Span::styled(" ".repeat(hint_padding), Style::default().fg(ratatui::style::Color::Reset)),
            Span::styled(" │", Style::default().fg(THINKING_COLOR)),
        ]));

        // Bottom border: └─────────────────────┘  (1 + inner_width+2 + 1 = inner_width+4)
        lines.push(Line::from(vec![
            Span::styled("└", Style::default().fg(THINKING_COLOR)),
            Span::styled("─".repeat(inner_width + 2), Style::default().fg(THINKING_COLOR)),
            Span::styled("┘", Style::default().fg(THINKING_COLOR)),
        ]));

        // Blank line after bubble
        lines.push(Line::from(Span::raw("")));

        lines
    }
}

/// Truncate a span's content to a maximum visual width, appending "…" if truncated.
///
/// This prevents content lines from exceeding the bubble's inner_width,
/// which would push the right border │ off-screen.
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

/// Format tool output content for display.
///
/// If the content is JSON, pretty-print it with syntax highlighting.
/// Otherwise, wrap plain text with the appropriate color.
/// Empty/whitespace content shows a success indicator instead of blank output.
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
