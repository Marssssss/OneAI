//! Session TAB strip (issue #30) — a one-row, **display-only** horizontal tab
//! bar at the top of the TUI showing up to 7 sessions, the active one
//! highlighted, so the user can see recent sessions at a glance.
//!
//! Switching sessions is deliberately NOT wired to the TAB strip (the user
//! found click-to-switch landed on the wrong session and duplicated rows).
//! To switch, use `/session resume <id>` — the single session-load codepath
//! (`tui::resume_session`). The strip mirrors the sidebar's session styling
//! (`CONTEXT_SESSION_COLOR` for active, `INACTIVE_SESSION_COLOR` otherwise).

use crate::tui::custom_terminal::Frame;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Tabs},
};

use super::super::app::App;
use super::super::theme::{CONTEXT_SESSION_COLOR, INACTIVE_SESSION_COLOR, INPUT_BORDER};

/// Maximum number of session tabs to render.
const MAX_TABS: usize = 7;

/// Draw the session tab strip (display-only).
///
/// Renders nothing when `rect` is empty (callers gate on
/// `sessions.len() > 1` and a 1-line height, so this is a defensive no-op).
pub fn draw_session_tabs(f: &mut Frame, rect: Rect, app: &App) {
    if rect.height == 0 {
        return;
    }

    // Show the most recent MAX_TABS sessions. `app.sessions` is append-ordered
    // (chronological); the active session may sit anywhere (startup DB load
    // prepends it, `/session resume` appends it), so sort the window with the
    // active entry first for a stable, predictable highlight position.
    let mut window: Vec<&crate::tui::app::SessionInfo> =
        app.sessions.iter().rev().take(MAX_TABS).collect();
    if window.is_empty() {
        return;
    }
    // Stable sort: active first (`!is_active`: active→false sorts before
    // inactive→true), the rest keep recency order.
    window.sort_by_key(|s| !s.is_active);

    let active_idx = window.iter().position(|s| s.is_active).unwrap_or(0);

    let titles: Vec<Line> = window
        .iter()
        .map(|s| {
            let indicator = if s.is_active { "●" } else { "○" };
            let label = format!("{} {}", indicator, s.short_id);
            let style = if s.is_active {
                Style::default()
                    .fg(CONTEXT_SESSION_COLOR)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(INACTIVE_SESSION_COLOR)
            };
            Line::from(vec![Span::styled(label, style)])
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(active_idx)
        .style(Style::default())
        .divider(ratatui::symbols::DOT)
        .highlight_style(
            Style::default()
                .fg(CONTEXT_SESSION_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(INPUT_BORDER)),
        );

    f.render_widget(tabs, rect);
}
