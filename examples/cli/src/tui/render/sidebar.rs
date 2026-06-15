//! Sidebar rendering — tools list, session info, context area, and four sections.
//!
//! The sidebar displays:
//! - Top: Context area (provider, session, paradigm, cost)
//! - Sessions section (multi-session list with active indicator)
//! - Tools section (with permission labels and active highlight)
//! - Paradigm section (active/inactive indicators)
//! - Cost section (token usage, cost, budget progress bar)

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use super::super::app::App;
use super::super::theme::*;

use oneai_agent::ParadigmKind;
use oneai_skill::builtin::skill_icon;

/// Draw the sidebar.
pub fn draw_sidebar(f: &mut Frame, rect: Rect, app: &App) {
    if rect.width == 0 {
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();

    // ── Context area (top) ──────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        format!(" {}", app.provider_info),
        Style::default().fg(CONTEXT_PROVIDER_COLOR).add_modifier(Modifier::BOLD),
    ))));

    items.push(ListItem::new(Line::from(Span::styled(
        format!(" ● {}", &app.session_id[..8.min(app.session_id.len())]),
        Style::default().fg(CONTEXT_SESSION_COLOR),
    ))));

    let paradigm_name = paradigm_display_name(&app.active_paradigm);
    items.push(ListItem::new(Line::from(Span::styled(
        format!(" ▸ {}#{}", paradigm_name, app.current_iteration),
        Style::default().fg(CONTEXT_PARADIGM_COLOR),
    ))));

    let token_display = app.token_usage.format_display();
    let cost_prefix = if app.session_cost_is_estimated { "~" } else { "" };
    items.push(ListItem::new(Line::from(Span::styled(
        format!(" 📊{} {}${:.3}", token_display, cost_prefix, app.session_cost),
        Style::default().fg(CONTEXT_COST_COLOR),
    ))));

    // ── Separator ────────────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " ──────────",
        Style::default().fg(SIDEBAR_BORDER),
    ))));

    // ── Sessions section ─────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " 💬 Sessions",
        Style::default().fg(SIDEBAR_TITLE_COLOR).add_modifier(Modifier::BOLD),
    ))));

    for session in &app.sessions {
        let indicator = if session.is_active { "●" } else { "○" };
        let style = if session.is_active {
            Style::default().fg(CONTEXT_SESSION_COLOR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(INACTIVE_SESSION_COLOR)
        };

        let preview = if session.preview.is_empty() {
            String::new()
        } else {
            format!(" {}", session.preview)
        };

        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {} {}", indicator, session.short_id),
                style,
            ),
            Span::styled(
                preview,
                Style::default().fg(ratatui::style::Color::DarkGray),
            ),
            Span::styled(
                format!(" ({})", session.message_count),
                Style::default().fg(ratatui::style::Color::DarkGray),
            ),
        ])));
    }

    // ── Separator ────────────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " ──────────",
        Style::default().fg(SIDEBAR_BORDER),
    ))));

    // ── Tools section ────────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " 🛠 Tools",
        Style::default().fg(SIDEBAR_TITLE_COLOR).add_modifier(Modifier::BOLD),
    ))));

    // Find which tool(s) are currently being called (by checking ToolCall messages)
    let active_tools: Vec<String> = app.messages.iter()
        .rev()
        .take_while(|m| matches!(m.role, ChatRole::ToolCall { .. } | ChatRole::Thinking))
        .filter_map(|m| {
            if let ChatRole::ToolCall { tool_name, .. } = &m.role {
                Some(tool_name.clone())
            } else {
                None
            }
        })
        .collect();

    for name in &app.tool_names {
        let perm_label = permission_label(name);
        let is_active = active_tools.contains(name);
        let prefix = if is_active { "⚡" } else { " •" };
        let name_style = if is_active {
            Style::default().fg(ACTIVE_TOOL_COLOR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(INACTIVE_TOOL_COLOR)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("{} {} ", prefix, name),
                name_style,
            ),
            Span::styled(
                perm_label.to_string(),
                Style::default().fg(ratatui::style::Color::DarkGray),
            ),
        ])));
    }

    // ── Paradigm section ─────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " ──────────",
        Style::default().fg(SIDEBAR_BORDER),
    ))));

    items.push(ListItem::new(Line::from(Span::styled(
        " 🔄 Paradigm",
        Style::default().fg(SIDEBAR_TITLE_COLOR).add_modifier(Modifier::BOLD),
    ))));

    let paradigms = [
        (ParadigmKind::ReAct, "ReAct"),
        (ParadigmKind::Plan, "Plan"),
        (ParadigmKind::Reflect, "Reflect"),
        (ParadigmKind::Explore, "Explore"),
    ];

    for (kind, name) in paradigms {
        let is_active = kind == app.active_paradigm;
        let indicator = if is_active { "▸" } else { " " };
        let style = if is_active {
            Style::default().fg(ACTIVE_PARADIGM_COLOR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(INACTIVE_PARADIGM_COLOR)
        };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(" {} {}", indicator, name),
            style,
        ))));
    }

    // ── Skills section ───────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " ──────────",
        Style::default().fg(SIDEBAR_BORDER),
    ))));

    let skill_count = app.skill_names.len();
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            " 🎯 Skills",
            Style::default().fg(SIDEBAR_TITLE_COLOR).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({})", skill_count),
            Style::default().fg(ratatui::style::Color::DarkGray),
        ),
    ])));

    // Display first 5 skills, with active skill highlighted
    let max_display = 5;
    let display_names: Vec<&String> = app.skill_names.iter().take(max_display).collect();
    let overflow_count = skill_count.saturating_sub(max_display);

    for name in display_names {
        let is_active = app.active_skill.as_deref() == Some(name.as_str());
        let icon = skill_icon(name);
        let indicator = if is_active { "▸" } else { "•" };
        let style = if is_active {
            Style::default().fg(CONTEXT_PARADIGM_COLOR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(INACTIVE_TOOL_COLOR)
        };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(" {} {} {}", indicator, icon, name),
            style,
        ))));
    }

    if overflow_count > 0 {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("   ...and {} more", overflow_count),
            Style::default().fg(ratatui::style::Color::DarkGray),
        ))));
    }

    // ── Cost section ─────────────────────────────────────────────────────
    items.push(ListItem::new(Line::from(Span::styled(
        " ──────────",
        Style::default().fg(SIDEBAR_BORDER),
    ))));

    items.push(ListItem::new(Line::from(Span::styled(
        " 💰 Cost",
        Style::default().fg(SIDEBAR_TITLE_COLOR).add_modifier(Modifier::BOLD),
    ))));

    let cost_prefix = if app.session_cost_is_estimated { "~" } else { "" };
    items.push(ListItem::new(Line::from(Span::styled(
        format!("  {}${:.4}", cost_prefix, app.session_cost),
        Style::default().fg(CONTEXT_COST_COLOR),
    ))));

    let total_tokens = app.token_usage.total;
    let prompt_tokens = app.token_usage.prompt;
    let completion_tokens = app.token_usage.completion;
    let estimated_prefix = if app.token_usage.is_estimated { "~" } else { "" };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            format!("  📊 {}{} total", estimated_prefix, format_token_count(total_tokens)),
            Style::default().fg(ratatui::style::Color::Gray),
        ),
    ])));
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            format!("  {}↑{} ↓{}", estimated_prefix, format_token_count(prompt_tokens), format_token_count(completion_tokens)),
            Style::default().fg(ratatui::style::Color::Gray),
        ),
    ])));

    // Context usage display — show token usage vs context window
    let ctx_used = total_tokens;
    let ctx_max = app.context_window_size;
    let ctx_ratio = if ctx_max > 0 { ctx_used as f64 / ctx_max as f64 } else { 0.0 };
    let ctx_pct = (ctx_ratio * 100.0).round() as u32;
    let ctx_warning = if ctx_pct > 80 { "⚠️" } else if ctx_pct > 50 { "⚡" } else { "" };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            format!("  📝 ctx {}%", ctx_pct),
            Style::default().fg(if ctx_pct > 80 {
                ratatui::style::Color::Rgb(255, 80, 80) // Red warning
            } else if ctx_pct > 50 {
                ratatui::style::Color::Rgb(255, 220, 80) // Yellow caution
            } else {
                CONTEXT_COST_COLOR
            }),
        ),
        Span::styled(
            format!(" {}/{}", format_token_count(ctx_used), format_token_count_u32(ctx_max)),
            Style::default().fg(ratatui::style::Color::Gray),
        ),
        Span::styled(
            ctx_warning,
            Style::default().fg(ratatui::style::Color::Rgb(255, 80, 80)),
        ),
    ])));

    // Budget progress bar (if we have a budget configured)
    let budget_bar = render_budget_bar(app, 20);
    items.push(ListItem::new(Line::from(budget_bar)));

    let sidebar = List::new(items)
        .block(Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(SIDEBAR_BORDER)));

    f.render_widget(sidebar, rect);
}

/// Render a simple progress bar for budget usage.
fn render_budget_bar(app: &App, width: usize) -> Vec<Span<'static>> {
    // Use token usage as a proxy for budget consumption
    // Show a simple bar: [████░░░░░░░░░░░░] 1.2k/10k
    let max_budget = 10000; // 10k tokens as visual max (not actual budget)
    let used = app.token_usage.total;
    let ratio = if max_budget > 0 {
        (used as f64 / max_budget as f64).min(1.0)
    } else {
        0.0
    };
    let filled = (ratio * width as f64).round() as usize;
    let empty = width - filled;

    vec![
        Span::styled("  [", Style::default().fg(SIDEBAR_BORDER)),
        Span::styled("█".repeat(filled), Style::default().fg(PROGRESS_FILL)),
        Span::styled("░".repeat(empty), Style::default().fg(PROGRESS_BG)),
        Span::styled("]", Style::default().fg(SIDEBAR_BORDER)),
        Span::styled(
            format!(" {}", format_token_count(used)),
            Style::default().fg(CONTEXT_COST_COLOR),
        ),
    ]
}

/// Get the display name for a paradigm kind.
fn paradigm_display_name(kind: &oneai_agent::ParadigmKind) -> &str {
    match kind {
        ParadigmKind::ReAct => "ReAct",
        ParadigmKind::Plan => "Plan",
        ParadigmKind::Reflect => "Reflect",
        ParadigmKind::Explore => "Explore",
    }
}

/// Get a permission label for a tool name (R/S/F).
fn permission_label(tool_name: &str) -> &str {
    match tool_name {
        "shell" => "[F]",
        "file_write" | "edit" => "[S]",
        "calculator" | "grep" | "glob" => "[R]",
        _ => "[S]",
    }
}

/// Format token count for display.
fn format_token_count(count: u32) -> String {
    format_token_count_u32(count)
}

/// Format token count for display (u32 version).
fn format_token_count_u32(count: u32) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else if count > 0 {
        format!("{}", count)
    } else {
        "0".to_string()
    }
}

use super::super::app::ChatRole;
