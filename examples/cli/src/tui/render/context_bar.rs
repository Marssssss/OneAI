//! Context bar — compact info line displayed when sidebar is collapsed.
//!
//! When the sidebar is hidden (Tab toggled), this bar shows essential
//! context information below the brand line in a compact format:
//! `provider·model | session_id | paradigm#iteration | tokens $cost`

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    Frame,
};

use super::super::app::App;
use super::super::theme::*;

/// Draw the context bar (shown only when sidebar is collapsed).
///
/// Displays a compact info line below the brand line:
/// `阿里百炼·qwen-plus | a3f2 | ReAct#3 | 1.2k $0.003`
#[allow(dead_code)]
pub fn draw_context_bar(f: &mut Frame, rect: Rect, app: &App) {
    let token_display = app.token_usage.format_display();
    let cost_prefix = if app.session_cost_is_estimated { "~" } else { "" };

    let line = Line::from(vec![
        Span::styled(" ", Style::default().fg(CONTEXT_PROVIDER_COLOR)),
        Span::styled(app.provider_info.clone(), Style::default().fg(CONTEXT_PROVIDER_COLOR).add_modifier(Modifier::BOLD)),
        Span::styled(" | ", Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(&app.session_id[..8.min(app.session_id.len())], Style::default().fg(CONTEXT_SESSION_COLOR)),
        Span::styled(" | ", Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(format!("{}#{}", paradigm_display_name(&app.active_paradigm), app.current_iteration),
            Style::default().fg(CONTEXT_PARADIGM_COLOR)),
        Span::styled(" | ", Style::default().fg(ratatui::style::Color::DarkGray)),
        Span::styled(format!("{} {}${:.3}", token_display, cost_prefix, app.session_cost),
            Style::default().fg(CONTEXT_COST_COLOR)),
    ]);

    let paragraph = ratatui::widgets::Paragraph::new(line)
        .style(Style::default().bg(BRAND_BG));

    f.render_widget(paragraph, rect);
}

fn paradigm_display_name(kind: &oneai_agent::ParadigmKind) -> &str {
    match kind {
        oneai_agent::ParadigmKind::ReAct => "ReAct",
        oneai_agent::ParadigmKind::Plan => "Plan",
        oneai_agent::ParadigmKind::Reflect => "Reflect",
        oneai_agent::ParadigmKind::Explore => "Explore",
    }
}
