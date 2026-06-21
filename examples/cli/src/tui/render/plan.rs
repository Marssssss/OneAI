//! Plan panel rendering — the persistent checklist above the chat area.
//!
//! Shows the live task plan (from `App.plan_state`) with each step's status
//! icon, description, and active-form label. Re-renders in place as the model
//! flips step statuses via the `task_*` control tools. When the model submits
//! a plan via `exit_plan_mode`, `draw_plan_approval` shows an accept/reject
//! popup instead.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use oneai_agent::{PlanStep, PlanStepStatus};

use super::super::app::App;
use super::super::theme::*;

/// How many lines the plan panel occupies. 0 when there's no plan.
pub fn plan_panel_height(app: &App) -> u16 {
    match &app.plan_state {
        Some(plan) if !plan.steps.is_empty() => {
            // 2 border lines + 1 header line + 1 per step, capped.
            let n = plan.steps.len() as u16 + 3;
            n.min(10)
        }
        _ => 0,
    }
}

/// Draw the persistent plan checklist panel.
pub fn draw_plan_panel(f: &mut Frame, rect: Rect, app: &App) {
    let plan = match &app.plan_state {
        Some(p) if !p.steps.is_empty() => p,
        _ => return,
    };

    let header = format!(
        "📋 Plan  ✓{}  ◐{}  ○{}  ✗{}",
        plan.count_by_status(PlanStepStatus::Completed),
        plan.count_by_status(PlanStepStatus::InProgress),
        plan.count_by_status(PlanStepStatus::Pending),
        plan.count_by_status(PlanStepStatus::Failed),
    );

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .title(Span::styled(header, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));

    let items: Vec<ListItem> = plan
        .steps
        .iter()
        .map(|step| ListItem::new(render_step_line(step)))
        .collect();

    let list = List::new(items)
        .block(block)
        .style(Style::default().fg(TEXT));

    // Highlight the in-progress step.
    let active_idx = plan.steps.iter().position(|s| s.status == PlanStepStatus::InProgress);
    let mut state = ListState::default();
    if let Some(i) = active_idx {
        state.select(Some(i));
    }
    f.render_widget(list, rect);
    // (ListState selection only changes highlight style; we don't need to
    // render a separate cursor, but keeping state lets us emphasize the row.)
}

fn render_step_line(step: &PlanStep) -> Line<'static> {
    let (icon, color) = match step.status {
        PlanStepStatus::Pending => ("○", DIM),
        PlanStepStatus::InProgress => ("◐", WARNING),
        PlanStepStatus::Completed => ("●", SUCCESS),
        PlanStepStatus::Failed => ("✗", DANGER),
    };
    let active = step
        .active_form
        .as_deref()
        .map(|a| format!(" ⟶ {}", a))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled(format!("{} ", icon), Style::default().fg(color)),
        Span::styled(format!("[{}] ", step.id), Style::default().fg(DIM)),
        Span::styled(step.description.clone(), Style::default().fg(if step.status == PlanStepStatus::Completed { DIM } else { TEXT })),
        Span::styled(active, Style::default().fg(color)),
    ])
}

/// Draw the exit_plan_mode accept/reject popup (Phase 3 gate).
///
/// Overlays the screen centered. Left/right arrow or Tab selects; Enter
/// confirms. Accept → the plan becomes the task list and execution proceeds;
/// Reject → plan mode stays on for re-planning. Input handling is in `app.rs`.
pub fn draw_plan_approval(f: &mut Frame, area: Rect, app: &App) {
    let (plan_text, steps, _reply_tx) = match &app.pending_plan {
        Some(p) => p,
        None => return,
    };

    let width = 64.min(area.width.saturating_sub(4));
    let height = (steps.len() as u16 + 10).min(area.height.saturating_sub(4));
    let popup_rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    // Clear the area underneath.
    f.render_widget(Clear, popup_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            "📋 Plan submitted — review",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(ACCENT));

    let inner = block.inner(popup_rect);
    f.render_widget(block, popup_rect);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),          // plan text + steps
            Constraint::Length(1),       // buttons row
        ])
        .split(inner);

    // Plan text (wrapped) + steps summary.
    let mut lines: Vec<Line> = Vec::new();
    for raw in plan_text.lines().take(height.saturating_sub(5) as usize) {
        let span: Span = Span::styled(raw.to_string(), Style::default().fg(TEXT));
        lines.push(Line::from(span));
    }
    if !steps.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Steps ({}):", steps.len()),
            Style::default().fg(DIM),
        )));
        for s in steps.iter().take(6) {
            lines.push(Line::from(vec![
                Span::styled("○ ", Style::default().fg(DIM)),
                Span::styled(format!("[{}] ", s.id), Style::default().fg(DIM)),
                Span::styled(s.description.clone(), Style::default().fg(TEXT)),
            ]));
        }
        if steps.len() > 6 {
            lines.push(Line::from(Span::styled(
                format!("  …+{} more", steps.len() - 6),
                Style::default().fg(DIM),
            )));
        }
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: true });
    f.render_widget(para, chunks[0]);

    // Buttons.
    let opts = [("Accept (execute)", SUCCESS), ("Reject (re-plan)", DANGER)];
    let mut spans: Vec<Span> = Vec::new();
    for (i, (label, color)) in opts.iter().enumerate() {
        let selected = i == app.plan_approval_selected_index;
        let style = if selected {
            Style::default().fg(*color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(*color)
        };
        spans.push(Span::styled(format!(" {} ", label), style));
        spans.push(Span::raw("   "));
    }
    let buttons = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    f.render_widget(buttons, chunks[1]);
}
