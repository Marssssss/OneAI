//! Render module — draws the full TUI layout.
//!
//! Layout structure:
//! ```
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ 品牌行 (1行)                                                      │
//! ├──────────┬───────────────────────────────────────────────────────┤
//! │ 侧栏24列 │  聊天区域 (Min)                                        │
//! │          ├───────────────────────────────────────────────────────┤
//! │          │  输入区 (3行)                                            │
//! └──────────┴───────────────────────────────────────────────────────┘
//! ```

use crate::tui::custom_terminal::Frame;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem},
};

use super::app::App;
use super::theme::*;

pub mod approval;
pub mod brand;
pub mod chat;
pub mod context_bar;
pub mod diff;
pub mod input;
pub mod markdown;
pub mod message;
pub mod plan;
pub mod session_tabs;
pub mod sidebar;
pub mod spinner;

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &mut App) {
    // 全屏清屏：ratatui 差分渲染只写 widget 显式覆盖的 cell，行尾超出内容、
    // 以及未被任何 widget 覆盖的 cell 都会保留上一帧字形。sidebar/input/
    // context_bar 等区域渲染的是不填满宽度、内容又快速变化（流式 cost/token、
    // 打字）的行，行变短时尾部无人重写 → 上一帧尾字残留（ghosting）。
    // 在每帧绘制前先全屏 Clear，保证所有 cell 重置为空白，从根上消除整类残留。
    // 廉价：仅写内存 buffer，差分后只有真正变化的 cell 才发往终端。
    f.render_widget(Clear, f.area());

    let total_size = f.area();

    // Determine brand line height: 5 lines for block art (large terminal), 1 line for text
    let brand_lines = if total_size.width >= 80 && total_size.height >= 30 {
        5
    } else {
        1
    };
    let context_bar_lines = if !app.show_sidebar { 1 } else { 0 };
    // Session tab strip (issue #30): one row when ≥2 sessions exist, else 0
    // (avoids stealing a line from the chat area when there's nothing to show).
    let tabs_lines = if app.sessions.len() > 1 { 1 } else { 0 };

    // Fixed 4-slot vertical layout: brand | session tabs | context bar | main.
    // Absent slots use Length(0) (a zero-height rect) so indices stay stable.
    let outer_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(brand_lines),       // brand line (1 or 5)
            Constraint::Length(tabs_lines),        // session tabs (0 or 1)
            Constraint::Length(context_bar_lines), // context bar (0 or 1)
            Constraint::Min(0),                    // main content
        ])
        .split(total_size);

    let brand_rect = outer_layout[0];
    let tabs_rect = outer_layout[1];
    let context_bar_rect = outer_layout[2];
    let content_rect = outer_layout[3];

    // Draw brand line
    brand::draw_brand(f, brand_rect, app);

    // Draw session tab strip (only when there's room and ≥2 sessions).
    if tabs_lines > 0 && tabs_rect.height > 0 {
        session_tabs::draw_session_tabs(f, tabs_rect, app);
    }

    // Draw context bar when sidebar is hidden
    if !app.show_sidebar && context_bar_rect.height > 0 {
        context_bar::draw_context_bar(f, context_bar_rect, app);
    }

    // Main content: sidebar | (chat + input)
    let main_layout = if app.show_sidebar {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(0)])
            .split(content_rect)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(0), Constraint::Min(0)])
            .split(content_rect)
    };

    let sidebar_rect = main_layout[0];
    let right_rect = main_layout[1];

    // Right panel: plan bar (optional) | chat | input
    //
    // The plan bar is a persistent checklist shown above the chat area whenever
    // a task plan exists (created via task_create / exit_plan_mode). It tracks
    // live progress as the model flips step statuses. When a plan is submitted
    // via exit_plan_mode, a floating accept/reject popup overlays instead.
    let plan_lines = plan::plan_panel_height(app);
    // Dynamic input height: grow with the wrapped line count so multi-line
    // input (Ctrl+Enter / bracketed paste) and long soft-wrapped lines stay
    // inside the input box instead of overflowing into the chat area
    // (issue #8). Capped at 40% of the screen; floored at 3 (border + 1 line
    // + hint). +2 accounts for the top border and the hint line.
    let input_visual = input::input_visual_line_count(&app.input, right_rect.width as usize);
    let max_input_height = ((total_size.height as usize) * 2 / 5).max(3);
    let input_height = (input_visual + 2).min(max_input_height).max(3) as u16;
    let panel_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if plan_lines > 0 {
            vec![
                Constraint::Length(plan_lines),   // plan bar
                Constraint::Min(0),               // chat area
                Constraint::Length(input_height), // input box (grows with content)
            ]
        } else {
            vec![
                Constraint::Length(0),            // no plan bar
                Constraint::Min(0),               // chat area
                Constraint::Length(input_height), // input box (grows with content)
            ]
        })
        .split(right_rect);

    draw_sidebar(f, sidebar_rect, app);
    if plan_lines > 0 {
        plan::draw_plan_panel(f, panel_layout[0], app);
    }
    chat::draw_chat(f, panel_layout[1], app);
    input::draw_input(f, panel_layout[2], app);

    // Draw command autocomplete popup (if active)
    if app.command_autocomplete && !app.input.is_empty() && app.input.starts_with('/') {
        draw_command_popup(f, panel_layout[1], panel_layout[2], app);
    }

    // Draw plan-decision popup (request_plan_decision) if pending — drawn
    // above the plan-review popup since a decision must resolve first.
    if app.plan_decision_pending.is_some() {
        plan::draw_plan_decision(f, total_size, app);
    }

    // Draw plan review popup (exit_plan_mode gate) if a plan is pending.
    if app.pending_plan.is_some() {
        plan::draw_plan_approval(f, total_size, app);
    }
}

/// Format a work duration as a compact stopwatch string.
///
/// - `< 60s`  → `"12s"`
/// - `< 1h`   → `"1:23"`
/// - `>= 1h`  → `"1:02:03"`
///
/// Used by the brand line, context bar, and sidebar to show how long the
/// current (or most recent) agent run has been working.
pub fn format_work_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }
}

/// Draw the sidebar (delegates to sidebar module).
fn draw_sidebar(f: &mut Frame, rect: Rect, app: &App) {
    sidebar::draw_sidebar(f, rect, app);
}

/// Draw command autocomplete popup.
///
/// Shows a floating list of matching commands at the bottom of the chat area,
/// just above the input box. The selected command is highlighted with a
/// prominent ▸ indicator and reversed (highlight) style for maximum visibility.
fn draw_command_popup(f: &mut Frame, chat_rect: Rect, _input_rect: Rect, app: &App) {
    let suggestions = app.get_command_suggestions();
    if suggestions.is_empty() {
        return;
    }

    // Clamp selected index to valid range
    let selected = app.command_autocomplete_index.min(suggestions.len() - 1);

    // Show at most 8 suggestions at a time
    let max_visible = 8;
    let total_count = suggestions.len();
    let visible_count = total_count.min(max_visible);
    let popup_height = visible_count as u16 + 2; // +2 for border

    // Calculate scroll offset so the selected item is always visible
    let scroll_offset = if selected >= max_visible {
        selected - max_visible + 1
    } else {
        0
    };

    // Position the popup at the bottom of the chat area, above the input box
    let popup_rect = Rect {
        x: chat_rect.x + 2,
        y: chat_rect.y + chat_rect.height.saturating_sub(popup_height),
        width: 50.min(chat_rect.width.saturating_sub(4)),
        height: popup_height.min(chat_rect.height),
    };

    // Clear the area before rendering (so it floats above chat content)
    f.render_widget(Clear, popup_rect);

    // Build list items with prominent selection indicator
    let items: Vec<ListItem> = suggestions
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(i, (cmd, desc))| {
            let is_selected = i == selected;

            // Selected item: ▸ indicator + bold cmd + reversed background
            // Non-selected item: blank prefix + normal cmd + dim desc
            let indicator = if is_selected { "▸ " } else { "  " };
            let indicator_style = if is_selected {
                Style::default()
                    .fg(ratatui::style::Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ratatui::style::Color::DarkGray)
            };
            let cmd_style = if is_selected {
                Style::default()
                    .fg(INPUT_PROMPT_COLOR)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(INPUT_TEXT_COLOR)
            };
            let desc_style = if is_selected {
                Style::default()
                    .fg(INPUT_HINT_COLOR)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ratatui::style::Color::DarkGray)
            };

            ListItem::new(Line::from(vec![
                Span::styled(indicator, indicator_style),
                Span::styled(format!("{} ", cmd), cmd_style),
                Span::styled(desc.to_string(), desc_style),
            ]))
        })
        .collect();

    // Show scroll indicator if there are more items than visible
    let title_suffix = if total_count > max_visible {
        format!(" ({}/{})", selected + 1, total_count)
    } else {
        String::new()
    };

    // Title reflects two-level autocomplete (issue #30): a suggestion with a
    // space (`/session resume`) is a subcommand; otherwise it's a top-level
    // command.
    let is_subcommand = suggestions
        .first()
        .map(|(cmd, _)| cmd.contains(' '))
        .unwrap_or(false);
    let title_label = if is_subcommand {
        "Subcommands"
    } else {
        "Commands"
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(INPUT_BORDER))
            .title(Span::styled(
                format!(" {}{} ", title_label, title_suffix),
                Style::default()
                    .fg(INPUT_PROMPT_COLOR)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(BRAND_BG)),
    );

    f.render_widget(list, popup_rect);
}

// ─── Issue #26 regression tests ─────────────────────────────────────────────
// Tool output must be visually distinct from the model's prose (assistant text
// is sage green; success tool output is cool slate), and code diffs must follow
// programmer convention (green `+`, red `-`).
#[cfg(test)]
mod issue26_tests {
    use super::super::theme::*;
    use super::diff::render_diff_lines;
    use super::message::render_result_lines;

    /// Find the foreground Color of the `+`/`-` marker span of a diff line
    /// (the marker is the span whose content is exactly "+" or "-").
    fn marker_fg(spans: &[ratatui::text::Span], marker: &str) -> Option<ratatui::style::Color> {
        spans
            .iter()
            .find(|s| s.content == marker)
            .and_then(|s| s.style.fg)
    }

    /// Extract the foreground Color from the first styled, non-empty span of a
    /// line, ignoring the 2-space indent and line-number gutter.
    fn first_fg(spans: &[ratatui::text::Span]) -> Option<ratatui::style::Color> {
        for s in spans {
            if s.content.trim().is_empty() {
                continue;
            }
            return s.style.fg;
        }
        None
    }

    /// The core bug: success tool-output body used `TOOL_RESULT_SUCCESS_COLOR`
    /// (== ASSISTANT_COLOR == sage green), indistinguishable from the model's
    /// prose. It must now use a distinct color.
    #[test]
    fn tool_output_body_differs_from_assistant_prose() {
        assert_ne!(
            TOOL_OUTPUT_COLOR, ASSISTANT_COLOR,
            "tool output must not share the assistant prose color"
        );
        // And specifically must differ from the success status color (sage green).
        assert_ne!(TOOL_OUTPUT_COLOR, TOOL_RESULT_SUCCESS_COLOR);
    }

    /// Success tool output renders its body in TOOL_OUTPUT_COLOR, not the
    /// success/assistant green.
    #[test]
    fn success_tool_output_body_uses_output_color() {
        let lines = render_result_lines("some stdout line\nmore output", true, 80);
        assert!(!lines.is_empty());
        for line in &lines {
            let fg = first_fg(&line.spans).unwrap_or(TOOL_OUTPUT_COLOR);
            assert_eq!(
                fg, TOOL_OUTPUT_COLOR,
                "success tool output body must be slate, not green"
            );
        }
    }

    /// Failure tool output keeps the danger red so errors still stand out.
    #[test]
    fn failure_tool_output_body_stays_red() {
        let lines = render_result_lines("boom: something failed", false, 80);
        assert!(!lines.is_empty());
        let fg = first_fg(&lines[0].spans).unwrap_or(TOOL_RESULT_FAILURE_COLOR);
        assert_eq!(fg, TOOL_RESULT_FAILURE_COLOR);
    }

    /// Programmer-convention diff colors: `+` additions green, `-` deletions
    /// red, rendered via explicit RGB (terminal-palette independent).
    #[test]
    fn diff_additions_green_deletions_red() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n ctx\n";
        let lines = render_diff_lines(diff, 80);

        // Find the added (+new) and deleted (-old) lines by inspecting content.
        let mut added_fg = None;
        let mut deleted_fg = None;
        for line in &lines {
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if joined.contains("+new") {
                added_fg = marker_fg(&line.spans, "+");
            } else if joined.contains("-old") {
                deleted_fg = marker_fg(&line.spans, "-");
            }
        }
        assert_eq!(
            added_fg,
            Some(DIFF_ADDED_FG),
            "added diff lines must be green"
        );
        assert_eq!(
            deleted_fg,
            Some(DIFF_DELETED_FG),
            "deleted diff lines must be red"
        );
        // Green/red are themselves distinct colors (sanity).
        assert_ne!(DIFF_ADDED_FG, DIFF_DELETED_FG);
    }
}
