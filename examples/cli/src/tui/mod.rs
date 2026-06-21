//! TUI (Terminal User Interface) for the OneAI CLI demo.
//!
//! Provides an interactive terminal interface with:
//! - Brand line: gradient "OneAI" with spinner when thinking
//! - Left sidebar: context area + tools + paradigm + cost sections
//! - Right panel: scrollable chat area with bubble-style messages
//! - Input box: single-line with Enter=send, Esc=quit, Tab=sidebar

use std::sync::Arc;
use std::collections::HashMap;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};

use oneai_app::AppBuilder;
use oneai_core::ModelConfig;
use oneai_provider::ProviderFactory;
use oneai_tool::CalculatorTool;
use oneai_agent::ParadigmKind;
use oneai_core::ApprovalResponse;
use oneai_domain::coding_pack;

use app::{App, ApprovalPendingState, ChatRole, TokenUsage};
use observer::{ObserverEvent, TuiObserver};
use render::spinner::advance_frame;
use session::SessionState;

// ─── Public Modules ────────────────────────────────────────────────────────

pub mod app;
pub mod observer;
pub mod render;
pub mod session;
pub mod history;
pub mod input_mode;
pub mod theme;

// ─── Run TUI ────────────────────────────────────────────────────────────────

/// Run the TUI application.
///
/// Sets up crossterm (raw mode + alternate screen), creates the ratatui
/// terminal, and runs the main event loop.
pub fn run_tui(
    provider_config: Option<ModelConfig>,
    domain_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Setup panic hook to restore terminal state if we crash
    let original_hook = Arc::new(std::panic::take_hook());
    std::panic::set_hook(Box::new({
        let hook = original_hook.clone();
        move |panic_info| {
            // Try to restore terminal state before printing the panic
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
            let _ = std::io::stdout().execute(LeaveAlternateScreen);
            // Call the original hook to print the panic message
            hook(panic_info);
        }
    }));

    // Setup terminal — enable mouse capture for scroll wheel support.
    // Text selection works via mouse drag in chat area: drag to select → release to copy.
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Build App
    let provider_info = match &provider_config {
        Some(config) => {
            let url = config.base_url.as_deref().unwrap_or("default");
            let model = config.model_name.as_deref().unwrap_or("unknown");
            let detected = if url.contains("dashscope") { "阿里百炼" }
                else if url.contains("deepseek") { "DeepSeek" }
                else if url.contains("anthropic") { "Anthropic" }
                else if url.contains("localhost") || url.contains("127.0.0.1") { "Ollama" }
                else if url.contains("openai") || url == "default" { "OpenAI" }
                else { "OpenAI-compatible" };
            format!("{detected} · {model}")
        }
        None => "No Provider".to_string(),
    };
    let model_name = match &provider_config {
        Some(config) => config.model_name.as_deref().unwrap_or("unknown").to_string(),
        None => "unknown".to_string(),
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (app, session_state, approval_rx) = rt.block_on(async {
        // Use ChannelApprovalGateWithThreshold — Medium risk auto-approves,
        // High risk requires human approval via the TUI
        // Use default_rate_limiter — pre-call throttle with common provider limits
        // (prevents exceeding API rate limits; provider-level retry handles 429 after)
        let (builder, approval_rx) = AppBuilder::new()
            .default_parser()
            .default_rate_limiter()
            .channel_approval_gate(16, oneai_core::RiskLevel::Medium);

        let mut builder = builder;
        if let Some(config) = provider_config {
            let provider = ProviderFactory::create(config);
            builder = builder.provider(Arc::from(provider));
        }

        let app = builder.build().await.expect("App build failed");

        // The skill registry is shared between the AgentLoop (always-on skill
        // menu), the `skill` tool (on-demand prompt loading), and the TUI.
        // Register built-in skills onto it FIRST so the SkillTool sees them.
        let domain_pack_name = domain_name.unwrap_or("coding");
        let skills = oneai_skill::builtin::skills_for_domain(domain_pack_name);
        app.skill_registry.register_builtin(skills).await.unwrap();

        // Register domain-specific tools from the selected domain pack
        let domain = crate::cmd_pack::get_builtin_pack(domain_pack_name, ".")
            .unwrap_or_else(|| {
                // Try loading from installed packs or project directory
                oneai_domain::domain_pack_from_dir(".").unwrap_or_else(|_| coding_pack("."))
            });
        for tool in &domain.tools {
            app.register_tool(tool.clone()).await.unwrap();
        }
        // Also register CalculatorTool as a general-purpose tool
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        // Register the `skill` tool — gives the model a call path to load a
        // skill's full prompt (progressive disclosure Tier2/Tier3).
        app.register_tool(Arc::new(oneai_agent::SkillTool::new(app.skill_registry.clone())))
            .await
            .unwrap();

        let tool_names = app.tool_executor().list_tools().await;
        let session = app.create_session();
        let session_id = session.session_id().to_string();

        let app_arc = Arc::new(app);
        let session_state = SessionState { app: app_arc.clone(), session };
        let session_state = Arc::new(tokio::sync::Mutex::new(session_state));

        let mut tui_app = App::new(
            provider_info,
            model_name,
            tool_names,
            session_id,
            app_arc.skill_registry.clone(),
        );

        tui_app.skill_names = tui_app.skill_registry.skill_names().await;
        tui_app.current_domain = domain_pack_name.to_string();

        (tui_app, session_state, approval_rx)
    });

    // Channel for observer events
    let (observer_tx, observer_rx) = tokio::sync::mpsc::unbounded_channel();

    // Standalone interrupt slot — holds the running AgentLoop so the TUI can
    // request an interrupt (Esc) WITHOUT the session lock (which is held for
    // the whole run_agent). Independent Arc → no deadlock.
    let interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // Run the main loop
    let result = run_main_loop(&mut terminal, app, session_state, observer_tx, observer_rx, &rt, approval_rx, interrupt_slot);

    // Restore terminal
    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Dispatch a crossterm event to the appropriate handler.
fn dispatch_event(
    app: &mut App,
    event: Event,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    observer_tx: &tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
    rt: &tokio::runtime::Runtime,
    interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
) {
    match event {
        Event::Key(key) => handle_key_event(app, key, session_state, observer_tx, rt, interrupt_slot),
        Event::Mouse(mouse) => handle_mouse_event(app, mouse),
        _ => {}
    }
}

/// Cycle the interaction mode (Shift+Tab) and announce the new mode.
fn cycle_interaction_mode(app: &mut App) {
    app.interaction_mode = app.interaction_mode.next();
    let hint = match app.interaction_mode {
        app::InteractionMode::Normal => {
            "⚡ Normal mode: approvals required for high-risk tools (Shift+Tab to switch)"
        }
        app::InteractionMode::AutoAccept => {
            "⚡ Auto-accept mode: all tool calls approved silently (Shift+Tab to switch)"
        }
        app::InteractionMode::Plan => {
            "⚡ Plan mode: tool execution disabled — agent will only produce a plan (Shift+Tab to switch)"
        }
    };
    app.add_message(ChatRole::System, hint);
    app.dirty = true;
}

/// Handle a single crossterm key event.
fn handle_key_event(
    app: &mut App,
    key: KeyEvent,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    observer_tx: &tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
    rt: &tokio::runtime::Runtime,
    interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
) {
    // Shift+Tab / Shift+Tab — cycle interaction mode (Normal → Auto → Plan).
    // Handled globally so it works even while an approval card is shown.
    if key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::BackTab | KeyCode::Tab)
    {
        cycle_interaction_mode(app);
        return;
    }

    if app.approval_pending.is_some() {
        handle_approval_key(app, key);
        return;
    }
    // Plan accept/reject gate (exit_plan_mode) — handled before input so the
    // user can't type while a decision is pending.
    if app.pending_plan.is_some() {
        handle_plan_approval_key(app, key);
        return;
    }
    if app.search_mode {
        handle_search_key(app, key);
        app.dirty = true;
        return;
    }
    // Esc while the agent is running (and not in vim multi-line edit) →
    // request an instant interrupt. The cancel token aborts the in-flight
    // inference/stream; the loop pauses at the next boundary.
    if app.is_thinking
        && matches!(app.input_mode, input_mode::InputMode::SingleLine)
        && key.kind == KeyEventKind::Press
        && key.code == KeyCode::Esc
    {
        rt.block_on(async {
            if let Some(agent_loop) = interrupt_slot.lock().await.as_ref() {
                agent_loop.request_interrupt(oneai_core::InterruptReason::Custom {
                    reason: "User requested interrupt".to_string(),
                });
                app.add_message(
                    ChatRole::System,
                    "⏸ Interrupted — type additional context, then Enter to resume.",
                );
            }
        });
        app.dirty = true;
        return;
    }
    let msg = app.handle_key_event(key);
    if let Some(user_input) = msg {
        if !app.is_thinking {
            handle_user_input_async(app, session_state.clone(), user_input, observer_tx, rt, interrupt_slot);
        }
    }
}

/// Handle a single crossterm mouse event.
///
/// Supports:
/// 1. **Scroll wheel** — scrolls chat content up/down
/// 2. **Scrollbar drag/click** — drags scrollbar thumb or jumps to position
fn handle_mouse_event(app: &mut App, mouse_event: crossterm::event::MouseEvent) {
    let chat_rect = app.last_chat_rect;
    let is_in_chat = chat_rect.width > 0
        && mouse_event.row >= chat_rect.y
        && mouse_event.row < chat_rect.y + chat_rect.height;
    let is_in_scrollbar = is_in_chat
        && mouse_event.column >= chat_rect.x + chat_rect.width - 2
        && mouse_event.column <= chat_rect.x + chat_rect.width;

    match mouse_event.kind {
        // ── Scroll wheel ──────────────────────────────────────────────────────
        crossterm::event::MouseEventKind::ScrollDown => {
            app.chat_scroll_y = app.chat_scroll_y.saturating_add(3);
            app.user_scrolled = true;
            app.dirty = true;
        }
        crossterm::event::MouseEventKind::ScrollUp => {
            app.chat_scroll_y = app.chat_scroll_y.saturating_sub(3);
            app.user_scrolled = true;
            app.dirty = true;
        }

        // ── Left button down (scrollbar or chat-area collapse toggle) ──────
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if is_in_scrollbar {
                // Scrollbar click — jump to position
                let y_offset = (mouse_event.row - chat_rect.y) as usize;
                let track_height = chat_rect.height as usize;
                let content_height = app.content_height;
                let viewport_height = chat_rect.height as usize;
                if track_height > 0 && content_height > viewport_height {
                    let ratio = y_offset as f64 / track_height as f64;
                    app.chat_scroll_y = ((ratio * content_height as f64) as usize)
                        .min(content_height.saturating_sub(viewport_height));
                    app.user_scrolled = true;
                    app.dirty = true;
                }
            } else if is_in_chat {
                // Chat-area click — toggle collapse for the message under the cursor.
                // Map the screen row to a content line, then walk messages (summing
                // each message's rendered line count) to find which one was clicked.
                let clicked_line = (mouse_event.row as usize)
                    .saturating_sub(chat_rect.y as usize)
                    .saturating_add(app.chat_scroll_y);

                let mut line_cursor = 0usize;
                let mut target: Option<String> = None;
                for msg in app.messages.iter() {
                    let msg_height = app.render_cache.entries.get(&msg.id)
                        .map(|c| c.lines.len())
                        .unwrap_or_else(|| {
                            // Cache miss — render transiently just to count lines.
                            let width = (chat_rect.width as usize).saturating_sub(1);
                            let is_collapsed = app.collapsed_ids.contains(&msg.id);
                            render::message::render_message_lines(
                                msg, is_collapsed, width, app.spinner_frame, app.approval_selected_index,
                            ).len().max(1)
                        });

                    if clicked_line < line_cursor + msg_height {
                        // Click landed inside this message — toggle only if the
                        // message's content is long enough to be collapsible
                        // (> COLLAPSE_THRESHOLD wrapped lines). Short content
                        // renders in full and is not foldable.
                        let width = (chat_rect.width as usize).saturating_sub(1);
                        let collapsible = render::message::message_is_collapsible(msg, width);
                        if collapsible {
                            target = Some(msg.id.clone());
                        }
                        break;
                    }
                    line_cursor += msg_height;
                }
                // Mutate after the immutable borrow over app.messages ends.
                if let Some(id) = target {
                    app.toggle_collapse(&id);
                    app.render_cache.invalidate(&id);
                    app.dirty = true;
                }
            }
        }

        // ── Left button drag (scrollbar only) ────────────────────────────────
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
            if is_in_scrollbar {
                // Scrollbar drag — map Y position to scroll offset
                let y_offset = (mouse_event.row - chat_rect.y) as usize;
                let track_height = chat_rect.height as usize;
                let content_height = app.content_height;
                let viewport_height = chat_rect.height as usize;
                if track_height > 0 && content_height > viewport_height {
                    let ratio = y_offset as f64 / track_height as f64;
                    app.chat_scroll_y = ((ratio * content_height as f64) as usize)
                        .min(content_height.saturating_sub(viewport_height));
                    app.user_scrolled = true;
                    app.dirty = true;
                }
            }
        }

        _ => {}
    }
}

/// Main TUI event loop.
///
/// The agent runs in a background tokio task, and observer events
/// are processed asynchronously in the TUI loop. This enables
/// the typewriter effect — stream chunks arrive in real-time
/// while the TUI continues rendering.
fn run_main_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut app: App,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    observer_tx: tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
    mut observer_rx: tokio::sync::mpsc::UnboundedReceiver<ObserverEvent>,
    rt: &tokio::runtime::Runtime,
    mut approval_rx: tokio::sync::mpsc::Receiver<oneai_tool::ApprovalPendingItem>,
    interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    while !app.should_quit {
        // Advance spinner frame only when thinking (spinner animation needs periodic redraw)
        if app.is_thinking {
            app.spinner_frame = advance_frame(app.spinner_frame);
            app.dirty = true;
        }

        // Stream throttle: flush buffered chunks at ~10fps (100ms interval)
        // This prevents 100+ redraws/second during fast streaming
        if !app.stream_buffer.is_empty() {
            const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
            if app.last_stream_flush.elapsed() >= STREAM_FLUSH_INTERVAL {
                app.flush_stream_buffer();
            }
        }

        // Poll for events — use shorter interval when autocomplete is active
        // for more responsive typing/navigation (16ms ≈ 60fps feel)
        // Otherwise use 50ms to reduce CPU usage when idle
        let poll_interval = if app.command_autocomplete {
            std::time::Duration::from_millis(16)
        } else {
            std::time::Duration::from_millis(50)
        };

        if event::poll(poll_interval)? {
            let first_event = event::read()?;
            dispatch_event(&mut app, first_event, session_state.clone(), &observer_tx, rt, interrupt_slot.clone());

            // Drain all pending events (especially scroll events) in one frame
            while event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    dispatch_event(&mut app, ev, session_state.clone(), &observer_tx, rt, interrupt_slot.clone());
                } else {
                    break;
                }
            }
        }

        // Process observer events (streaming/typewriter works here)
        while let Ok(event) = observer_rx.try_recv() {
            process_observer_event(&mut app, event);
        }

        // Process approval requests from the approval gate
        if let Ok(item) = approval_rx.try_recv() {
            let tool_name = item.request.tool_name.clone();
            let justification = item.request.justification.clone();
            let perm_label = item.request.permission_level.map(|p| format!("{:?}", p))
                .unwrap_or_else(|| format!("{:?}", item.request.risk_level));

            // Auto-accept mode — silently approve every tool call (no message, no card).
            if matches!(app.interaction_mode, app::InteractionMode::AutoAccept) {
                let response = oneai_core::ApprovalResponse::Approved { modified_args: None };
                let _ = item.response_tx.send(response);
            // Check if the tool is in the session allowlist
            } else if app.session_allowlist.contains(&tool_name) {
                // Auto-approve for this session
                let response = oneai_core::ApprovalResponse::Approved { modified_args: None };
                let _ = item.response_tx.send(response);
                app.add_message(ChatRole::System, format!("Auto-approved {} (session allowlist)", tool_name));
            } else {
                // Show approval card in the TUI
                app.approval_pending = Some(ApprovalPendingState {
                    request: item.request.clone(),
                    response_tx: Some(item.response_tx),
                    tool_name,
                    justification,
                });
                app.add_message(ChatRole::Approval, format!(
                    "Tool: {} ({})\n{}",
                    app.approval_pending.as_ref().unwrap().tool_name,
                    perm_label,
                    app.approval_pending.as_ref().unwrap().justification,
                ));
            }
        }

        // Render AFTER processing all events and state changes
        // This ensures visual feedback appears immediately after key presses,
        // without waiting for the next poll cycle.
        if app.dirty {
            terminal.draw(|f| render::draw(f, &mut app))?;
            app.dirty = false;
        }
    }

    Ok(())
}

/// Handle a user input message — send to agent or handle command.
fn paradigm_display_name(kind: &ParadigmKind) -> &str {
    match kind {
        ParadigmKind::ReAct => "ReAct",
        ParadigmKind::Plan => "Plan",
        ParadigmKind::Reflect => "Reflect",
        ParadigmKind::Explore => "Explore",
        _ => "Unknown",
    }
}

fn handle_user_input_async(
    app: &mut App,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    input: String,
    observer_tx: &tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
    rt: &tokio::runtime::Runtime,
    interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
) {
    let trimmed = input.trim();

    // Handle commands synchronously
    if trimmed.starts_with('/') {
        let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
        let cmd = parts[0];

        match cmd {
            "/quit" | "/q" => {
                app.should_quit = true;
                return;
            }
            "/help" | "/h" => {
                app.add_message(ChatRole::System, "Commands:\n  /help · /tools · /skills · /skill · /clear · /cost · /context · /session · /paradigm · /domain · /compact · /wf · /tool · /new · /quit\nKeys: Enter=send, Ctrl+Enter=newline, Tab=sidebar, Esc=vim/quit, ↑↓=history, Ctrl+↑↓/PageUp/PageDown=scroll\nMouse: drag to select & copy text, scroll wheel to scroll, drag scrollbar to jump\nSkills: /skill <name> activate · /skill off deactivate · /skill add <name> <desc>\nWorkflows: /wf list · /wf run <name> · /wf show <name> · /wf graph <name> · /wf status · /wf history\nContext: /context shows detailed token breakdown by category");
                return;
            }
            "/tools" | "/t" => {
                let tools = app.tool_names.iter()
                    .map(|n| format!("  • {}", n))
                    .collect::<Vec<_>>()
                    .join("\n");
                app.add_message(ChatRole::System, format!("Registered tools:\n{}", tools));
                return;
            }
            "/clear" => {
                app.messages.clear();
                app.render_cache.invalidate_all();
                rt.block_on(async {
                    session_state.lock().await.reset_session();
                });
                app.session_id = rt.block_on(async {
                    session_state.lock().await.session.session_id().to_string()
                });
                app.token_usage = TokenUsage::new();
                app.context_tokens = 0;
                app.context_tokens_is_estimated = false;
                app.session_cost = 0.0;
                app.session_cost_is_estimated = false;
                app.current_iteration = 0;
                app.last_context_accounting = None;
                // Create a new session entry in the sidebar
                app.add_new_session(app.session_id.clone());
                app.add_message(ChatRole::System, "Conversation cleared.");
                return;
            }
            "/cost" => {
                let ctx_est = if app.context_tokens_is_estimated { "~" } else { "" };
                app.add_message(ChatRole::System, format!(
                    "Session cost: {}${:.4}\nContext: {}{} / {}k tokens",
                    if app.session_cost_is_estimated { "~" } else { "" },
                    app.session_cost, ctx_est, app.context_tokens, app.context_window_size / 1000
                ));
                return;
            }
            "/context" => {
                // Use stored accounting from AgentLoop when available — this
                // reflects the actual assembled context (system prompt, tool defs,
                // domain pack, etc.), not the bare session conversation.
                // Only fall back to session conversation if no agent run has occurred.
                if let Some(accounting) = &app.last_context_accounting {
                    app.add_message(ChatRole::System, accounting.format_display(&app.provider_info));
                } else {
                    // No agent run yet — compute from session conversation (best available)
                    let accounting = rt.block_on(async {
                        let state = session_state.lock().await;
                        let conv = state.session.conversation();
                        oneai_core::ContextAccounting::account(
                            conv,
                            &app.model_name,
                            app.tool_names.len(),
                        )
                    });
                    app.context_tokens = accounting.total_tokens;
                    app.context_tokens_is_estimated = accounting.is_estimated;
                    app.context_window_size = accounting.context_window_size;
                    app.last_context_accounting = Some(accounting.clone());
                    app.add_message(ChatRole::System, accounting.format_display(&app.provider_info));
                }
                app.dirty = true;
                return;
            }
            "/session" => {
                let ctx_est = if app.context_tokens_is_estimated { "~" } else { "" };
                let cost_est = if app.session_cost_is_estimated { "~" } else { "" };
                app.add_message(ChatRole::System, format!(
                    "Session ID: {}\nProvider: {}\nParadigm: {}#{}\nContext: {}{} / {}k\nCost: {}${:.4}",
                    app.session_id,
                    app.provider_info,
                    paradigm_display_name(&app.active_paradigm),
                    app.current_iteration,
                    ctx_est,
                    app.context_tokens,
                    app.context_window_size / 1000,
                    cost_est,
                    app.session_cost,
                ));
                return;
            }
            "/paradigm" => {
                if parts.len() < 2 {
                    app.add_message(ChatRole::System, format!(
                        "Current paradigm: {}\nAvailable: ReAct, Plan, Reflect, Explore\nUsage: /paradigm <name>",
                        paradigm_display_name(&app.active_paradigm),
                    ));
                    return;
                }
                let name = parts[1];
                match name {
                    "react" | "ReAct" => {
                        app.active_paradigm = ParadigmKind::ReAct;
                        app.add_message(ChatRole::System, "Switched to ReAct paradigm");
                    }
                    "plan" | "Plan" => {
                        app.active_paradigm = ParadigmKind::Plan;
                        app.add_message(ChatRole::System, "Switched to Plan paradigm");
                    }
                    "reflect" | "Reflect" => {
                        app.active_paradigm = ParadigmKind::Reflect;
                        app.add_message(ChatRole::System, "Switched to Reflect paradigm");
                    }
                    "explore" | "Explore" => {
                        app.active_paradigm = ParadigmKind::Explore;
                        app.add_message(ChatRole::System, "Switched to Explore paradigm");
                    }
                    _ => {
                        app.add_message(ChatRole::Error, format!("Unknown paradigm: {}. Available: ReAct, Plan, Reflect, Explore", name));
                    }
                }
                return;
            }
            "/domain" => {
                if parts.len() < 2 {
                    app.add_message(ChatRole::System, format!(
                        "Current domain: {}\nAvailable domains:\n  • coding — Software development (8+ tools)\n  • research — Research & analysis (7+ tools)\n  • general — General-purpose (calculator only)\n\nUsage: /domain <name>",
                        app.current_domain,
                    ));
                    return;
                }
                let name = parts[1];
                let domain = crate::cmd_pack::get_builtin_pack(name, ".");
                match domain {
                    Some(pack) => {
                        // Register domain tools
                        rt.block_on(async {
                            let state = session_state.lock().await;
                            for tool in &pack.tools {
                                state.app.register_tool(tool.clone()).await.unwrap();
                            }
                            // Also keep CalculatorTool as general-purpose
                            state.app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
                            app.tool_names = state.app.tool_executor().list_tools().await;
                            // Switch skills to domain
                            app.skill_registry.replace_all(oneai_skill::builtin::skills_for_domain(name)).await.unwrap();
                            app.skill_names = app.skill_registry.skill_names().await;
                            // Clear any active skill — the old domain's skill is gone.
                            state.session.set_active_skill(None).await;
                        });
                        app.current_domain = name.to_string();
                        app.active_skill = None;
                        let tool_names_str: Vec<String> = pack.tools.iter().map(|t| t.name().to_string()).collect();
                        app.add_message(ChatRole::System, format!(
                            "Switched to {} domain. Tools: {}\nSkills: {} domain skills + general skills",
                            name,
                            tool_names_str.join(", "),
                            name,
                        ));
                    }
                    None => {
                        app.add_message(ChatRole::Error, format!(
                            "Unknown domain: {}. Available: coding, research, general", name
                        ));
                    }
                }
                return;
            }
            "/new" => {
                // Create a new session (preserves old session in sidebar)
                rt.block_on(async {
                    session_state.lock().await.reset_session();
                });
                let new_session_id = rt.block_on(async {
                    session_state.lock().await.session.session_id().to_string()
                });
                app.messages.clear();
                app.render_cache.invalidate_all();
                app.token_usage = TokenUsage::new();
                app.context_tokens = 0;
                app.context_tokens_is_estimated = false;
                app.session_cost = 0.0;
                app.session_cost_is_estimated = false;
                app.current_iteration = 0;
                app.last_context_accounting = None;
                app.add_new_session(new_session_id);
                app.add_message(ChatRole::System, "New session created. Previous sessions preserved in sidebar.");
                return;
            }
            "/compact" => {
                // Compact conversation — preserve key messages, generate summary
                compact_conversation(app);
                // Also trigger backend session context compression
                rt.block_on(async {
                    session_state.lock().await.reset_session();
                });
                let new_session_id = rt.block_on(async {
                    session_state.lock().await.session.session_id().to_string()
                });
                app.add_new_session(new_session_id);
                app.add_message(ChatRole::System, "Conversation context compacted. Key messages preserved. New session created.");
                return;
            }
            "/skills" => {
                // List all registered skills with descriptions and keywords
                rt.block_on(async {
                    let skills = app.skill_registry.list().await;
                    if skills.is_empty() {
                        app.add_message(ChatRole::System, "No skills registered. Switch domain with /domain or add skills with /skill add <name> <description>");
                    } else {
                        let mut lines = vec![format!("🎯 Available Skills ({})\n", skills.len())];
                        for skill in &skills {
                            let icon = oneai_skill::builtin::skill_icon(&skill.name);
                            let active_marker = if app.active_skill.as_deref() == Some(&skill.name) {
                                " ▸ ACTIVE"
                            } else {
                                ""
                            };
                            lines.push(format!("  {} {} — {}{}", icon, skill.name, skill.description, active_marker));
                            lines.push(format!("     Keywords: {}", skill.trigger_keywords.join(", ")));
                        }
                        let active_info = if let Some(name) = &app.active_skill {
                            format!("\nActive: {}", name)
                        } else {
                            "\nActive: none".to_string()
                        };
                        lines.push(active_info);
                        lines.push("\nType /skill <name> to activate, /skill search <query> to find relevant skills".to_string());
                        app.add_message(ChatRole::System, lines.join("\n"));
                    }
                });
                return;
            }
            "/skill" => {
                // Skill sub-command dispatch: add, remove, info, search, off, <name>
                if parts.len() < 2 {
                    app.add_message(ChatRole::System, "Skill commands:\n  /skill <name>        — Activate a skill\n  /skill off            — Deactivate current skill\n  /skill add <name> <desc> — Register a custom skill\n  /skill remove <name>  — Remove a skill\n  /skill info <name>    — Show skill details\n  /skill search <query> — Find relevant skills");
                    return;
                }
                let sub_cmd = parts[1];

                match sub_cmd {
                    "off" => {
                        if let Some(name) = app.active_skill.take() {
                            // Sync to the shared session state read by run_agent.
                            rt.block_on(async {
                                session_state.lock().await.session.set_active_skill(None).await;
                            });
                            app.add_message(ChatRole::System, format!("✅ Skill deactivated: {}", name));
                        } else {
                            app.add_message(ChatRole::System, "No active skill to deactivate.");
                        }
                        app.dirty = true;
                        return;
                    }
                    "add" => {
                        // /skill add <name> <description>
                        // parts[0]="/skill", parts[1]="add", parts[2]="name description"
                        if parts.len() < 3 {
                            app.add_message(ChatRole::Error, "Usage: /skill add <name> <description>");
                            return;
                        }
                        // Split the third part into name + description
                        // The name is the first word, the rest is description
                        let add_args: Vec<&str> = parts[2].splitn(2, ' ').collect();
                        if add_args.len() < 2 {
                            app.add_message(ChatRole::Error, "Usage: /skill add <name> <description> (description required)");
                            return;
                        }
                        let skill_name = add_args[0];
                        let skill_desc = add_args[1];

                        let prompt_template = format!("Act as a {} expert. {}", skill_name, skill_desc);
                        let trigger_keywords = vec![skill_name.to_string()];

                        let skill = oneai_core::SkillDescriptor {
                            name: skill_name.to_string(),
                            description: skill_desc.to_string(),
                            prompt_template: prompt_template.clone(),
                            trigger_keywords,
                            embedding: None,
                        };

                        rt.block_on(async {
                            app.skill_registry.register(skill).await.unwrap();
                            app.skill_names = app.skill_registry.skill_names().await;
                        });
                        app.add_message(ChatRole::System, format!(
                            "✅ Skill registered: {}\n  Description: {}\n  Prompt: \"{}\"\n  Keywords: [{}]",
                            skill_name, skill_desc, prompt_template, skill_name
                        ));
                        app.dirty = true;
                        return;
                    }
                    "remove" => {
                        // /skill remove <name>
                        if parts.len() < 3 {
                            app.add_message(ChatRole::Error, "Usage: /skill remove <name>");
                            return;
                        }
                        let skill_name = parts[2];
                        // Don't allow removing the active skill
                        if app.active_skill.as_deref() == Some(skill_name) {
                            app.add_message(ChatRole::Error, format!("Cannot remove active skill '{}'. Deactivate first with /skill off.", skill_name));
                            return;
                        }
                        rt.block_on(async {
                            let existed = app.skill_registry.find_by_name(skill_name).await;
                            app.skill_registry.remove(skill_name).await.unwrap();
                            app.skill_names = app.skill_registry.skill_names().await;
                            if existed.is_some() {
                                app.add_message(ChatRole::System, format!("✅ Skill removed: {}", skill_name));
                            } else {
                                app.add_message(ChatRole::Error, format!("Skill '{}' not found.", skill_name));
                            }
                        });
                        app.dirty = true;
                        return;
                    }
                    "info" => {
                        // /skill info <name>
                        if parts.len() < 3 {
                            app.add_message(ChatRole::Error, "Usage: /skill info <name>");
                            return;
                        }
                        let skill_name = parts[2];
                        rt.block_on(async {
                            let skill = app.skill_registry.find_by_name(skill_name).await;
                            if let Some(s) = skill {
                                let icon = oneai_skill::builtin::skill_icon(&s.name);
                                let active = if app.active_skill.as_deref() == Some(&s.name) {
                                    " ▸ ACTIVE"
                                } else {
                                    ""
                                };
                                app.add_message(ChatRole::System, format!(
                                    "{} {} — {}{}\n\nPrompt template:\n{}\n\nTrigger keywords: [{}]",
                                    icon, s.name, s.description, active, s.prompt_template, s.trigger_keywords.join(", ")
                                ));
                            } else {
                                app.add_message(ChatRole::Error, format!("Skill '{}' not found.", skill_name));
                            }
                        });
                        return;
                    }
                    "search" => {
                        // /skill search <query>
                        if parts.len() < 3 {
                            app.add_message(ChatRole::Error, "Usage: /skill search <query>");
                            return;
                        }
                        let query = parts[2];
                        rt.block_on(async {
                            let skills = app.skill_registry.list().await;
                            let selector = oneai_skill::SkillSelector::new();
                            let matches = selector.select_skills(query, &skills).await.unwrap();
                            if matches.is_empty() {
                                app.add_message(ChatRole::System, format!("No skills matching '{}'.", query));
                            } else {
                                let mut lines = vec![format!("🎯 Skill search results for '{}':\n", query)];
                                for skill in &matches {
                                    let icon = oneai_skill::builtin::skill_icon(&skill.name);
                                    lines.push(format!("  {} {} — {}", icon, skill.name, skill.description));
                                }
                                lines.push(format!("\nTop match: {}", matches[0].name));
                                lines.push(format!("Type /skill {} to activate", matches[0].name));
                                app.add_message(ChatRole::System, lines.join("\n"));
                            }
                        });
                        return;
                    }
                    // /skill <name> — activate a skill by name
                    _ => {
                        let skill_name = sub_cmd;
                        let found = rt.block_on(async {
                            let skill = app.skill_registry.find_by_name(skill_name).await;
                            if let Some(s) = skill {
                                app.active_skill = Some(s.name.clone());
                                app.add_message(ChatRole::System, format!(
                                    "✅ Skill activated: {}\nPrompt injected: \"{}\"\nThe agent will now prioritize this skill's approach.\nType /skill off to deactivate, or /skill <other> to switch.",
                                    s.name, s.prompt_template
                                ));
                                true
                            } else {
                                app.add_message(ChatRole::Error, format!("Skill '{}' not found. Use /skills to see available skills.", skill_name));
                                false
                            }
                        });
                        // Sync the activated skill to the shared session state
                        // (read by run_agent to inject the prompt each turn).
                        if found {
                            let name_to_set = app.active_skill.clone();
                            rt.block_on(async {
                                session_state.lock().await.session.set_active_skill(name_to_set).await;
                            });
                        }
                        app.dirty = true;
                        return;
                    }
                }
            }
            "/tool" => {
                if parts.len() < 3 {
                    app.add_message(ChatRole::Error, "Usage: /tool <name> <json>");
                    return;
                }
                let tool_name = parts[1];
                let args_str = parts[2];
                let args: serde_json::Value = serde_json::from_str(args_str)
                    .unwrap_or(serde_json::json!({}));

                let result = rt.block_on(async {
                    session_state.lock().await.session.execute_tool(tool_name, args).await
                });

                match result {
                    Ok(output) => {
                        if output.success {
                            app.add_message(ChatRole::ToolInvocation {
                                call_id: String::new(),
                                tool_name: tool_name.to_string(),
                                args: String::new(),
                                result: Some((true, format!("{}: {}", tool_name, output.content))),
                            }, format!("{}: {}", tool_name, output.content));
                        } else {
                            let error_msg = output.error.as_deref().unwrap_or("unknown error").to_string();
                            app.add_message(ChatRole::ToolInvocation {
                                call_id: String::new(),
                                tool_name: tool_name.to_string(),
                                args: String::new(),
                                result: Some((false, format!("{}: {}", tool_name, error_msg))),
                            }, format!("{}: {}", tool_name, error_msg));
                        }
                    }
                    Err(e) => app.add_message(ChatRole::Error, format!("Error: {e}")),
                }
                return;
            }
            "/wf" => {
                handle_workflow_command(app, session_state, &parts, rt);
                return;
            }
            _ => {
                app.add_message(ChatRole::Error, format!("Unknown command: {cmd}"));
                return;
            }
        }
    }

    // Check if provider is available
    let has_provider = rt.block_on(async {
        session_state.lock().await.app.has_provider()
    });
    if !has_provider {
        app.add_message(ChatRole::Error, "No LLM provider configured. Set ONEAI_API_KEY and ONEAI_BASE_URL.");
        return;
    }

    // Send to agent — run in a background task for async observer events
    app.add_message(ChatRole::User, trimmed.to_string());
    // One-time tip on the first agent run: let the user know about mode switching.
    if !app.mode_prompt_shown {
        app.mode_prompt_shown = true;
        app.add_message(ChatRole::System, "Tip: press Shift+Tab to cycle modes — Normal → Auto-accept (silent approval) → Plan (no tool execution).");
    }
    // Sync the interaction mode into the session before launching the agent loop
    // (Plan mode blocks tool execution; Auto is handled at the approval layer).
    let plan_mode = matches!(app.interaction_mode, app::InteractionMode::Plan);
    rt.block_on(async {
        session_state.lock().await.session.set_plan_mode(plan_mode);
    });
    app.is_thinking = true;
    // Add a thinking bubble to show the agent is processing
    app.add_collapsed_message(ChatRole::Thinking, "Processing your request...");

    let task = trimmed.to_string();
    let tx1 = observer_tx.clone();
    let tx2 = observer_tx.clone();

    rt.spawn(async move {
        let mut state = session_state.lock().await;
        let observer = TuiObserver::new(tx1);

        let result = state.session.run_agent(&task, &observer, interrupt_slot.clone()).await;

        match result {
            Ok(agent_result) => {
                if !agent_result.completed {
                    let _ = tx2.send(ObserverEvent::Error(
                        format!("Agent did not reach a final answer after {} iterations.", agent_result.iterations)
                    ));
                }
                let _ = tx2.send(ObserverEvent::Complete(agent_result));
            }
            Err(e) => {
                let err_str = e.to_string();
                let _ = tx2.send(ObserverEvent::Error(format!("Error: {}", err_str)));
                if err_str.contains("API error") {
                    let _ = tx2.send(ObserverEvent::Error("Hint: check your ONEAI_API_KEY and ONEAI_BASE_URL.".to_string()));
                }
            }
        }
    });
}

/// Handle `/wf` workflow commands.
fn handle_workflow_command(
    app: &mut App,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    parts: &[&str],
    rt: &tokio::runtime::Runtime,
) {
    if parts.len() < 2 {
        app.add_message(ChatRole::System,
            "Workflow commands:\n\
             /wf list              — Show available workflows\n\
             /wf run <name>        — Execute a workflow\n\
             /wf run <name> --vars — Execute with variables\n\
             /wf define <json>     — Define & execute from JSON\n\
             /wf define --file <p> — Define & execute from file\n\
             /wf show <name>       — Show DAG visualization\n\
             /wf graph <name>      — Show StateGraph visualization\n\
             /wf status            — Show recent workflow results\n\
             /wf cancel            — Cancel running workflow (not yet supported)\n\
             /wf history           — Show workflow execution history");
        return;
    }

    match parts[1] {
        "list" => {
            let workflows = rt.block_on(async {
                session_state.lock().await.session.get_available_workflows()
            });
            let graph_names = rt.block_on(async {
                session_state.lock().await.session.get_state_graph_names()
            });

            if workflows.is_empty() && graph_names.is_empty() {
                app.add_message(ChatRole::System,
                    "No predefined workflows available. Use /domain coding to load coding workflows.");
            } else {
                let mut lines = vec![format!("📋 Available Workflows ({})\n", workflows.len())];
                for wf in &workflows {
                    lines.push(format!("  • {} — {}", wf.name, wf.description));
                }
                if !graph_names.is_empty() {
                    lines.push(format!("\n📋 Available StateGraphs ({})\n", graph_names.len()));
                    for name in &graph_names {
                        lines.push(format!("  • {}", name));
                    }
                }
                lines.push("\nType /wf run <name> to execute".to_string());
                app.add_message(ChatRole::System, lines.join("\n"));
            }
        }

        "run" => {
            if parts.len() < 3 {
                app.add_message(ChatRole::Error, "Usage: /wf run <name> [--vars key=value ...]");
                return;
            }
            let wf_name = parts[2];

            // Parse --vars arguments
            let mut vars = HashMap::new();
            let mut i = 3;
            while i < parts.len() {
                if parts[i] == "--vars" || parts[i].starts_with("--vars") {
                    i += 1;
                    while i < parts.len() && !parts[i].starts_with("--") {
                        if let Some((k, v)) = parts[i].split_once('=') {
                            vars.insert(k.to_string(), v.to_string());
                        }
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }

            // Try as workflow first, then as StateGraph
            let wf_config = rt.block_on(async {
                session_state.lock().await.session.get_workflow_config(wf_name)
            });

            if let Some(mut config) = wf_config {
                // Merge user variables
                for (k, v) in vars {
                    config.variables.insert(k, v);
                }

                app.add_message(ChatRole::System,
                    format!("▶ Starting workflow '{}' with {} steps...", config.name, config.steps.len()));

                let result = rt.block_on(async {
                    session_state.lock().await.session.execute_workflow(&config).await
                });

                match result {
                    Ok(result) => {
                        let mut lines = vec![format!("✅ Workflow '{}' completed", result.name)];
                        lines.push(format!("   Steps: {} total, {} completed, {} failed",
                            result.step_results.len(),
                            result.completed_steps().len(),
                            result.failed_steps().len()));
                        lines.push(format!("   Time: {}ms", result.total_time_ms));

                        for (step_id, step) in &result.step_results {
                            let status_icon = match step.status {
                                oneai_workflow::StepStatus::Completed => "✅",
                                oneai_workflow::StepStatus::Failed => "❌",
                                oneai_workflow::StepStatus::Skipped => "⏭️",
                                _ => "⏳",
                            };
                            let output_preview = step.output.as_ref()
                                .map(|o| {
                                    let truncated = if o.len() > 200 {
                                        format!("{}...", &o[..200])
                                    } else {
                                        o.clone()
                                    };
                                    truncated
                                })
                                .unwrap_or_else(|| "no output".to_string());
                            lines.push(format!("   {} {}: {}", status_icon, step_id, output_preview));
                        }

                        app.add_message(ChatRole::System, lines.join("\n"));
                    }
                    Err(e) => {
                        app.add_message(ChatRole::Error, format!("❌ Workflow failed: {}", e));
                    }
                }
            } else {
                // Try as StateGraph
                let graph = rt.block_on(async {
                    session_state.lock().await.session.get_state_graph(wf_name)
                });

                if let Some(graph) = graph {
                    app.add_message(ChatRole::System,
                        format!("▶ Starting StateGraph '{}' execution...", graph.name));

                    let result = rt.block_on(async {
                        session_state.lock().await.session.execute_state_graph(&graph).await
                    });

                    match result {
                        Ok(result) => {
                            let mut lines = vec![format!("✅ StateGraph '{}' completed", result.name)];
                            lines.push(format!("   Iterations: {}", result.iterations));
                            lines.push(format!("   Completed: {}", result.completed));
                            if let Some(terminal) = &result.terminal_node {
                                lines.push(format!("   Terminal node: {}", terminal));
                            }
                            if let Some(output) = &result.final_state.last_result {
                                let preview = if output.len() > 500 {
                                    format!("{}...", &output[..500])
                                } else {
                                    output.clone()
                                };
                                lines.push(format!("   Result: {}", preview));
                            }
                            app.add_message(ChatRole::System, lines.join("\n"));
                        }
                        Err(e) => {
                            app.add_message(ChatRole::Error, format!("❌ StateGraph failed: {}", e));
                        }
                    }
                } else {
                    app.add_message(ChatRole::Error,
                        format!("Workflow '{}' not found. Use /wf list to see available workflows.", wf_name));
                }
            }
        }

        "define" => {
            if parts.len() < 3 {
                app.add_message(ChatRole::Error,
                    "Usage: /wf define <json_string> OR /wf define --file <path>");
                return;
            }

            let config = if parts[2] == "--file" {
                if parts.len() < 4 {
                    app.add_message(ChatRole::Error, "Usage: /wf define --file <path>");
                    return;
                }
                let path = parts[3];
                let content = std::fs::read_to_string(path);
                match content {
                    Ok(content) => {
                        oneai_workflow::WorkflowConfig::from_json(&content)
                    }
                    Err(e) => {
                        app.add_message(ChatRole::Error, format!("Cannot read file: {}", e));
                        return;
                    }
                }
            } else {
                // Direct JSON string
                oneai_workflow::WorkflowConfig::from_json(parts[2])
            };

            match config {
                Ok(config) => {
                    app.add_message(ChatRole::System,
                        format!("▶ Executing custom workflow '{}' with {} steps...",
                            config.name, config.steps.len()));

                    let result = rt.block_on(async {
                        session_state.lock().await.session.execute_workflow(&config).await
                    });

                    match result {
                        Ok(result) => {
                            app.add_message(ChatRole::System,
                                format!("✅ Workflow '{}' completed. Steps: {}, Time: {}ms",
                                    result.name, result.step_results.len(), result.total_time_ms));
                        }
                        Err(e) => {
                            app.add_message(ChatRole::Error, format!("❌ Workflow failed: {}", e));
                        }
                    }
                }
                Err(e) => {
                    app.add_message(ChatRole::Error, format!("Invalid workflow definition: {}", e));
                }
            }
        }

        "show" => {
            if parts.len() < 3 {
                app.add_message(ChatRole::Error, "Usage: /wf show <name>");
                return;
            }
            let wf_name = parts[2];
            let config = rt.block_on(async {
                session_state.lock().await.session.get_workflow_config(wf_name)
            });

            match config {
                Some(config) => {
                    let viz = rt.block_on(async {
                        session_state.lock().await.session.render_workflow_dag(&config)
                    });
                    app.add_message(ChatRole::System, format!("Workflow: {}\n{}", config.name, viz));
                }
                None => {
                    app.add_message(ChatRole::Error, format!("Workflow '{}' not found.", wf_name));
                }
            }
        }

        "graph" => {
            if parts.len() < 3 {
                app.add_message(ChatRole::Error, "Usage: /wf graph <name>");
                return;
            }
            let graph_name = parts[2];
            let graph = rt.block_on(async {
                session_state.lock().await.session.get_state_graph(graph_name)
            });

            match graph {
                Some(graph) => {
                    let viz = rt.block_on(async {
                        session_state.lock().await.session.render_state_graph(&graph)
                    });
                    app.add_message(ChatRole::System, viz);
                }
                None => {
                    app.add_message(ChatRole::Error, format!("StateGraph '{}' not found.", graph_name));
                }
            }
        }

        "status" => {
            // Show the latest workflow execution result from history
            let history = rt.block_on(async {
                session_state.lock().await.session.workflow_history().to_vec()
            });

            if history.is_empty() {
                app.add_message(ChatRole::System,
                    "No workflow executions in this session. Use /wf run <name> to start one.");
            } else {
                let last = history.last().unwrap();
                let kind_str = match last.kind {
                    oneai_app::session::WorkflowKind::Dag => "DAG Workflow",
                    oneai_app::session::WorkflowKind::StateGraph => "StateGraph",
                };
                let status_icon = if last.success { "✅" } else { "❌" };
                app.add_message(ChatRole::System, format!(
                    "Last workflow execution:\n  {} {} ({})\n  Status: {} {}\n  Time: {}\n  Summary: {}",
                    status_icon, last.name, kind_str, status_icon,
                    if last.success { "completed" } else { "failed" },
                    last.timestamp, last.summary
                ));
            }
        }

        "history" => {
            // Show all workflow execution history
            let history = rt.block_on(async {
                session_state.lock().await.session.workflow_history().to_vec()
            });

            if history.is_empty() {
                app.add_message(ChatRole::System,
                    "No workflow executions in this session. Use /wf run <name> to start one.");
            } else {
                let mut lines = vec![format!("📋 Workflow Execution History ({})\n", history.len())];
                for (idx, entry) in history.iter().enumerate() {
                    let kind_str = match entry.kind {
                        oneai_app::session::WorkflowKind::Dag => "DAG",
                        oneai_app::session::WorkflowKind::StateGraph => "Graph",
                    };
                    let status_icon = if entry.success { "✅" } else { "❌" };
                    lines.push(format!(
                        "  {} {} [{}] {} — {} ({})",
                        status_icon, idx + 1, kind_str, entry.name,
                        entry.summary, entry.timestamp
                    ));
                }
                app.add_message(ChatRole::System, lines.join("\n"));
            }
        }

        "cancel" => {
            // /wf cancel — cancel a running workflow
            // This requires async cancellation which is not yet implemented.
            // Currently workflow execution is synchronous (blocking) within the
            // TUI event loop, so there's no way to cancel mid-execution.
            // Future implementation: use tokio::sync::CancellationToken in
            // StateGraphExecutor and WorkflowExecutor to support graceful cancellation.
            app.add_message(ChatRole::System,
                "⚠️ /wf cancel is not yet supported. Workflows execute synchronously \
                 within the TUI event loop and cannot be cancelled mid-execution.\n\n\
                 Future implementation will use CancellationToken for graceful \
                 cancellation of StateGraph and DAG workflows.");
        }

        _ => {
            app.add_message(ChatRole::Error, format!("Unknown workflow command: {}. Type /wf for help.", parts[1]));
        }
    }
}

/// Process an observer event and update the app state.
fn process_observer_event(app: &mut App, event: ObserverEvent) {
    match event {
        ObserverEvent::IterationStart(iteration, paradigm) => {
            app.current_iteration = iteration;
            app.active_paradigm = paradigm;
        }
        ObserverEvent::DirectAnswer(text) => {
            // Flush any buffered stream content FIRST — this ensures streaming
            // chunks are applied to an assistant message (or create one from
            // the buffer) before we decide whether DirectAnswer is a duplicate.
            // Without this flush, the stream_buffer remains full and the
            // already_has_assistant check returns false (no assistant bubble
            // exists yet), so DirectAnswer creates a new Assistant message.
            // Then Complete's flush_stream_buffer() finds the last message is
            // not Assistant (Checkpoint added an Iteration after it), and
            // creates a SECOND Assistant bubble from the buffer → two bubbles.
            app.flush_stream_buffer();

            // If streaming already created an assistant message, don't add a duplicate.
            // Search backwards through all messages in the current turn (until we hit
            // a User message, which marks the start of a new turn).
            let already_has_assistant = app.messages.iter().rev()
                .take_while(|m| m.role != ChatRole::User)
                .any(|m| m.role == ChatRole::Assistant);
            if already_has_assistant {
                // Streaming already showed this content — just ensure final redraw
                app.dirty = true;
            } else {
                // No streaming happened — add the direct answer as a new message
                app.add_message(ChatRole::Assistant, text);
            }
        }
        ObserverEvent::ToolCalls(calls) => {
            for call in calls {
                let args_str = serde_json::to_string_pretty(&call.args)
                    .unwrap_or_else(|_| call.args.to_string());

                // Dedup: in streaming mode the agent loop emits on_tool_calls
                // twice for the same call — once during streaming (when the
                // tool call is fully assembled) and again after the stream
                // completes (agent_loop.rs). Without dedup we'd add TWO
                // pending ToolInvocation cards; ToolResult only updates the
                // last one, leaving a stale ⏳ "preparation" card next to the
                // result card. Skip if a pending card for this call exists.
                let call_id = call.id.clone();
                let tool_name = call.name.clone();
                let already_pending = app.messages.iter().any(|m| {
                    if let ChatRole::ToolInvocation {
                        call_id: cid, tool_name: tn, args, result, ..
                    } = &m.role
                    {
                        if result.is_some() {
                            return false;
                        }
                        // Match by call_id when the provider assigned one;
                        // otherwise fall back to (tool_name, args) so empty-id
                        // calls still dedup correctly.
                        if !call_id.is_empty() {
                            cid == &call_id
                        } else {
                            cid.is_empty() && tn == &tool_name && args == &args_str
                        }
                    } else {
                        false
                    }
                });
                if already_pending {
                    continue;
                }

                // Add a unified ToolInvocation message — result is None (pending).
                // When ToolResult arrives, we'll UPDATE this same message.
                app.add_collapsed_message(
                    ChatRole::ToolInvocation {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        args: args_str,
                        result: None,
                    },
                    String::new(), // Content is empty until result arrives
                );
            }
        }
        ObserverEvent::ToolResult(call_id, tool_name, output) => {
            // Find the existing ToolInvocation message for this call_id and UPDATE it
            // with the result. This merges ToolCall + ToolResult into one message.
            let call_id_to_find = call_id.clone();
            let found_msg = app.messages.iter_mut().rev().find(|m| {
                if let ChatRole::ToolInvocation { call_id, result, .. } = &m.role {
                    call_id == &call_id_to_find && result.is_none()
                } else {
                    false
                }
            });

            if let Some(msg) = found_msg {
                // Update the existing ToolInvocation message with result
                let success = output.success;
                let result_content = if output.success {
                    if output.content.trim().is_empty() {
                        "(completed successfully)".to_string()
                    } else {
                        output.content.clone()
                    }
                } else {
                    format!("Error: {}", output.error.as_deref().unwrap_or("unknown error"))
                };

                // Update the role to include the result
                if let ChatRole::ToolInvocation { call_id, tool_name, args, result: _ } = &msg.role {
                    msg.role = ChatRole::ToolInvocation {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        result: Some((success, result_content.clone())),
                    };
                }
                msg.content = result_content.clone();

                // Decide collapsed state from the result's line count:
                // > COLLAPSE_THRESHOLD lines → collapsed (5-line preview + expand button)
                // ≤ COLLAPSE_THRESHOLD lines → expanded (rendered in full, not collapsible)
                if result_content.lines().count() > theme::COLLAPSE_THRESHOLD {
                    app.collapsed_ids.insert(msg.id.clone());
                } else {
                    app.collapsed_ids.remove(&msg.id);
                }
                // Invalidate render cache since content changed
                app.render_cache.invalidate(&msg.id);
                app.dirty = true;
            } else {
                // No matching ToolInvocation message found — this can happen
                // for /tool direct calls. Add a standalone result message.
                // `add_message` consults `default_collapsed`, which collapses
                // results longer than COLLAPSE_THRESHOLD lines (5-line preview)
                // and renders shorter ones in full.
                if output.success {
                    let result_content = if output.content.trim().is_empty() {
                        format!("{} completed successfully", tool_name)
                    } else {
                        output.content.clone()
                    };
                    app.add_message(
                        ChatRole::ToolInvocation {
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: String::new(),
                            result: Some((true, result_content.clone())),
                        },
                        result_content,
                    );
                } else {
                    app.add_message(
                        ChatRole::ToolInvocation {
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: String::new(),
                            result: Some((false, format!("Error: {}", output.error.as_deref().unwrap_or("unknown error")))),
                        },
                        format!("{}: {}", tool_name, output.error.as_deref().unwrap_or("unknown error")),
                    );
                }
            }
        }
        ObserverEvent::Delegate(task, agent_type) => {
            app.add_message(ChatRole::System, format!("delegating to {} sub-agent: {}", agent_type.name(), task));
        }
        ObserverEvent::ParadigmSwitch(paradigm) => {
            app.active_paradigm = paradigm;
            let name = match paradigm {
                ParadigmKind::Plan => "Plan",
                ParadigmKind::ReAct => "ReAct",
                ParadigmKind::Reflect => "Reflect",
                ParadigmKind::Explore => "Explore",
                _ => "Unknown",
            };
            app.add_message(ChatRole::System, format!("switching to {} paradigm", name));
        }
        ObserverEvent::Checkpoint(_iteration) => {
            // No visible message — checkpoint is silent in TUI
        }
        ObserverEvent::Complete(result) => {
            // Flush any remaining buffered stream text before marking as done
            app.flush_stream_buffer();
            app.is_thinking = false;
            // Remove useless thinking bubbles: the "Processing your request..."
            // placeholder (model never produced thinking) AND any empty thinking
            // bubble (model emitted an empty Thinking block). Both leave blank
            // cards that add clutter, so drop them on completion.
            app.messages.retain(|m| {
                if m.role == ChatRole::Thinking
                    && (m.content == "Processing your request..." || m.content.trim().is_empty())
                {
                    false // remove placeholder / empty thinking
                } else {
                    true
                }
            });

            // If the agent loop terminated without producing a meaningful final answer,
            // display a diagnostic message so the user knows what happened.
            // Common causes: streaming response failed, provider rejected request format,
            // or model produced empty response after tool calls.
            if result.final_answer.trim().is_empty() {
                let iterations = result.iterations;
                let completed = result.completed;
                let msg_content = if iterations <= 1 {
                    String::from(
                        "⚠️ Agent terminated without producing a response. \
                        This usually means the LLM provider returned an error or empty response. \
                        Possible causes:\n\
                        • Provider doesn't support tool calling with this model\n\
                        • Conversation format incompatible with the provider (智谱GLM/阿里百炼 require specific formats)\n\
                        • API rate limit or budget exceeded\n\
                        • Network error during streaming\n\
                        Try: check ONEAI_API_KEY and ONEAI_BASE_URL, verify the model supports tool calling."
                    )
                } else {
                    format!(
                        "⚠️ Agent terminated after {} iterations without a final answer (completed={}). \
                        The model may have produced an empty response after tool calls. \
                        This could indicate a provider format issue with tool results. \
                        Try: verify your provider supports multi-turn tool calling.",
                        iterations, completed
                    )
                };
                app.add_message(ChatRole::Error, msg_content);
            }

            app.dirty = true; // Need redraw to stop spinner
        }
        ObserverEvent::StreamChunk(text) => {
            app.append_to_last_assistant(&text);
        }
        ObserverEvent::Thinking(text) => {
            // Ignore empty/whitespace-only thinking fragments. Some providers
            // (e.g. GLM/阿里百炼) emit an empty Thinking content block as a
            // marker on rounds where the model produces no reasoning. Creating
            // a bubble for it would leave a blank thinking card that never
            // gets removed (it isn't the "Processing your request..." placeholder).
            if text.trim().is_empty() {
                return;
            }
            // Scope thinking per-iteration: only append to an existing
            // thinking bubble if it is still the TRAILING message (nothing has
            // been appended after it this round). If an assistant answer or
            // tool call already followed the last thinking block, a new
            // iteration has started — create a fresh thinking bubble so each
            // round of thinking stays attached to the answer it precedes,
            // instead of all thinking accumulating into the first round's
            // block at the top (which then scrolls off-screen).
            let is_trailing_thinking = app.messages.last()
                .map(|m| m.role == ChatRole::Thinking)
                .unwrap_or(false);
            if is_trailing_thinking {
                if let Some(thinking_msg) = app.messages.last_mut() {
                    if thinking_msg.content == "Processing your request..." {
                        // Replace placeholder with real thinking content
                        thinking_msg.content = text.clone();
                    } else {
                        // Append thinking fragment (streamed in chunks)
                        thinking_msg.content.push_str(&text);
                    }
                    // Invalidate render cache since content changed
                    app.render_cache.invalidate(&thinking_msg.id);
                    // Auto-expand thinking bubble when it has real content so user can see it
                    app.collapsed_ids.remove(&thinking_msg.id);
                }
            } else {
                // New round of thinking — create a fresh bubble (auto-scrolls to bottom)
                app.add_message(ChatRole::Thinking, text.clone());
            }
            app.dirty = true;
        }
        ObserverEvent::PlanUpdate(plan) => {
            // Live task list changed — update the persistent plan panel.
            // Only log a system message when the plan is first created (the
            // panel itself shows ongoing progress; per-status flips would spam
            // the chat).
            let was_none = app.plan_state.is_none();
            app.plan_state = plan;
            if was_none && app.plan_state.is_some() {
                if let Some(ps) = &app.plan_state {
                    app.add_message(ChatRole::System, format!(
                        "📋 Plan created — {} steps tracked in the panel above.",
                        ps.steps.len()
                    ));
                }
            }
            app.dirty = true;
        }
        ObserverEvent::PlanSubmitted { plan, steps, reply_tx } => {
            // exit_plan_mode — surface the plan for accept/reject (Phase 3 gate).
            // The AgentLoop is blocked awaiting `reply_tx`; the user's decision
            // (handled in handle_plan_approval_key) sends it.
            app.pending_plan = Some((plan, steps, Some(reply_tx)));
            app.plan_approval_selected_index = 0;
            app.is_thinking = false;
            app.dirty = true;
        }
        ObserverEvent::Error(msg) => {
            app.is_thinking = false;
            app.add_message(ChatRole::Error, msg);
        }
        // ObserverEvent::ApprovalRequest comes from the observer trait,
        // but actual approval flow uses ChannelApprovalGateWithThreshold
        // which sends ApprovalPendingItem directly. This is just an informational
        // event from the observer.
        ObserverEvent::ApprovalRequest(request) => {
            let perm_label = request.permission_level.map(|p| format!("{:?}", p))
                .unwrap_or_else(|| format!("{:?}", request.risk_level));
            app.add_message(ChatRole::Approval, format!(
                "⚠️ Tool: {} ({})\n{}",
                request.tool_name,
                perm_label,
                request.justification
            ));
        }
        ObserverEvent::ApprovalResponse(_response) => {
            app.approval_pending = None;
            app.dirty = true;
        }
        ObserverEvent::TokenUsageUpdate(usage) => {
            // Accumulate raw usage for cost tracking (some providers report real data, others 0)
            let iter_prompt = if usage.prompt > 0 { usage.prompt }
                else if !app.messages.is_empty() { app.estimate_tokens_from_messages() * 3 / 4 }
                else { 0 };
            let iter_completion = if usage.completion > 0 { usage.completion }
                else if !app.messages.is_empty() { app.estimate_tokens_from_messages() / 4 }
                else { 0 };
            let iter_total = iter_prompt + iter_completion;

            app.token_usage.prompt += iter_prompt;
            app.token_usage.completion += iter_completion;
            app.token_usage.total += iter_total;
            app.token_usage.is_estimated = usage.prompt == 0 && usage.completion == 0;

            // Estimate cost when we don't have real data
            if app.token_usage.is_estimated && iter_total > 0 {
                app.session_cost = App::estimate_cost_from_tokens(
                    app.token_usage.prompt, app.token_usage.completion
                );
                app.session_cost_is_estimated = true;
            }

            // Use real API prompt_tokens as context size when available.
            // This is the Claude Code approach — exact data from the API
            // overrides the heuristic estimate from ContextAccountingUpdate.
            // Flow: ContextAccountingUpdate (heuristic) → TokenUsageUpdate (exact, overrides)
            if usage.prompt > 0 {
                app.context_tokens = usage.prompt;
                app.context_tokens_is_estimated = false;
            }

            app.dirty = true; // Sidebar needs update
        }
        ObserverEvent::CostUpdate(cost) => {
            // cost is the cumulative session cost from the agent
            if cost > 0.0 {
                app.session_cost = cost;
                app.session_cost_is_estimated = false;
            }
            app.dirty = true; // Sidebar needs update
        }
        ObserverEvent::ContextAccountingUpdate(accounting) => {
            // Context accounting from the assembled inference request.
            // This is the REAL context breakdown — includes system prompt,
            // tool definitions, context sources, domain pack, etc.
            // Much more accurate than estimating from TUI-side messages alone.
            //
            // Store for the /context command — it reads this instead of
            // recomputing from the bare session conversation.
            app.last_context_accounting = Some(accounting.clone());

            // Update sidebar context display.
            // Note: TokenUsageUpdate (which fires AFTER inference) may
            // override context_tokens with exact API data if available.
            // Here we set the heuristic estimate as a baseline.
            app.context_tokens = accounting.total_tokens;
            app.context_tokens_is_estimated = accounting.is_estimated;
            app.context_window_size = accounting.context_window_size;
            app.dirty = true; // Sidebar needs update
        }
    }
}

/// Handle approval key presses (Y/N/M/A + cursor selection).
///
/// This is called from the main loop when an approval is pending.
/// Supports:
/// - Left/Right arrow: move selection cursor between options
/// - Enter: confirm currently selected option
/// - Y/N/M/A: direct shortcut (immediate response)
/// - Esc: cancel (deny)
fn handle_approval_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Handle cursor movement first (doesn't consume the pending state)
    match (key.modifiers, key.code) {
        // Left arrow: move selection cursor left (wraps from 0 to 3)
        (KeyModifiers::NONE, KeyCode::Left) => {
            if app.approval_selected_index == 0 {
                app.approval_selected_index = 3;
            } else {
                app.approval_selected_index -= 1;
            }
            app.dirty = true;
            // Invalidate approval message cache to re-render with new selection
            if let Some(last_approval_id) = app.messages.iter().rev()
                .find(|m| m.role == ChatRole::Approval)
                .map(|m| m.id.clone()) {
                app.render_cache.invalidate(&last_approval_id);
            }
            return;
        }
        // Right arrow: move selection cursor right (wraps from 3 to 0)
        (KeyModifiers::NONE, KeyCode::Right) => {
            if app.approval_selected_index >= 3 {
                app.approval_selected_index = 0;
            } else {
                app.approval_selected_index += 1;
            }
            app.dirty = true;
            if let Some(last_approval_id) = app.messages.iter().rev()
                .find(|m| m.role == ChatRole::Approval)
                .map(|m| m.id.clone()) {
                app.render_cache.invalidate(&last_approval_id);
            }
            return;
        }
        _ => {}
    }

    // Only process action keys if we have a pending state
    let pending = app.approval_pending.take();
    if let Some(state) = pending {
        let response = match (key.modifiers, key.code) {
            // Enter: confirm currently selected option
            (KeyModifiers::NONE, KeyCode::Enter) => {
                match app.approval_selected_index {
                    0 => { // Y: Approve
                        app.add_message(ChatRole::System, format!("✅ Approved: {}", state.tool_name));
                        ApprovalResponse::Approved { modified_args: None }
                    }
                    1 => { // N: Deny
                        app.add_message(ChatRole::System, format!("❌ Denied: {}", state.tool_name));
                        ApprovalResponse::Denied { reason: "User denied via TUI".to_string() }
                    }
                    2 => { // M: Modify
                        app.add_message(ChatRole::System, format!("📝 Modify requested for: {} (denied — modify not yet supported)", state.tool_name));
                        ApprovalResponse::Denied { reason: "User wants to modify — not yet supported in TUI".to_string() }
                    }
                    3 => { // A: Always
                        app.session_allowlist.insert(state.tool_name.clone());
                        app.add_message(ChatRole::System, format!("✅ Always approved: {} (added to session allowlist)", state.tool_name));
                        ApprovalResponse::Approved { modified_args: None }
                    }
                    _ => {
                        app.add_message(ChatRole::System, format!("✅ Approved: {}", state.tool_name));
                        ApprovalResponse::Approved { modified_args: None }
                    }
                }
            }

            // Y: Approve (shortcut)
            (KeyModifiers::NONE, KeyCode::Char('y')) | (KeyModifiers::NONE, KeyCode::Char('Y')) => {
                app.add_message(ChatRole::System, format!("✅ Approved: {}", state.tool_name));
                ApprovalResponse::Approved { modified_args: None }
            }

            // N: Deny (shortcut)
            (KeyModifiers::NONE, KeyCode::Char('n')) | (KeyModifiers::NONE, KeyCode::Char('N')) => {
                app.add_message(ChatRole::System, format!("❌ Denied: {}", state.tool_name));
                ApprovalResponse::Denied { reason: "User denied via TUI".to_string() }
            }

            // A: Always (shortcut)
            (KeyModifiers::NONE, KeyCode::Char('a')) | (KeyModifiers::NONE, KeyCode::Char('A')) => {
                app.session_allowlist.insert(state.tool_name.clone());
                app.add_message(ChatRole::System, format!("✅ Always approved: {} (added to session allowlist)", state.tool_name));
                ApprovalResponse::Approved { modified_args: None }
            }

            // M: Modify (shortcut)
            (KeyModifiers::NONE, KeyCode::Char('m')) | (KeyModifiers::NONE, KeyCode::Char('M')) => {
                app.add_message(ChatRole::System, format!("📝 Modify requested for: {} (denied — modify not yet supported)", state.tool_name));
                ApprovalResponse::Denied { reason: "User wants to modify — not yet supported in TUI".to_string() }
            }

            // Esc: deny and cancel
            (_, KeyCode::Esc) => {
                app.add_message(ChatRole::System, format!("❌ Cancelled: {}", state.tool_name));
                ApprovalResponse::Denied { reason: "User cancelled via TUI".to_string() }
            }

            // Unknown key: put the state back and ignore
            _ => {
                // Put the state back — we don't have a response yet
                app.approval_pending = Some(state);
                return;
            }
        };

        // Send the response back through the oneshot channel
        if let Some(tx) = state.response_tx {
            let _ = tx.send(response);
        }
    }
}

/// Handle keys while a plan (exit_plan_mode) is awaiting accept/reject.
///
/// ←/→ or Tab selects Accept/Reject; Enter confirms; Esc = reject. The
/// decision is sent to the blocked AgentLoop via the oneshot.
fn handle_plan_approval_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    // 0 = Accept, 1 = Reject
    let count = 2usize;
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Left)
        | (KeyModifiers::NONE, KeyCode::Tab)
        | (KeyModifiers::NONE, KeyCode::Right) => {
            app.plan_approval_selected_index = (app.plan_approval_selected_index + 1) % count;
            app.dirty = true;
        }
        (KeyModifiers::NONE, KeyCode::Enter) => {
            let accepted = app.plan_approval_selected_index == 0;
            if let Some((_plan, _steps, reply_tx)) = app.pending_plan.take() {
                if let Some(tx) = reply_tx {
                    let _ = tx.send(accepted);
                }
                if accepted {
                    app.add_message(ChatRole::System, "✅ Plan accepted — execution starting.");
                } else {
                    app.add_message(ChatRole::System, "↩️ Plan rejected — revise and re-submit (exit_plan_mode).");
                }
            }
            app.is_thinking = true; // loop resumes (was blocked on the gate)
            app.dirty = true;
        }
        (_, KeyCode::Esc) => {
            if let Some((_plan, _steps, reply_tx)) = app.pending_plan.take() {
                if let Some(tx) = reply_tx {
                    let _ = tx.send(false);
                }
                app.add_message(ChatRole::System, "↩️ Plan rejected (Esc) — revise and re-submit.");
            }
            app.dirty = true;
        }
        _ => {}
    }
}

/// Handle search mode key presses (Ctrl+F activated).
///
/// In search mode, the user types a query and matching messages are highlighted.
/// Enter navigates to the next result, Esc exits search mode.
fn handle_search_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    match (key.modifiers, key.code) {
        // Esc: exit search mode
        (_, KeyCode::Esc) => {
            app.search_mode = false;
            app.search_query.clear();
            app.search_results.clear();
            app.search_result_index = 0;
        }

        // Enter: jump to next search result
        (KeyModifiers::NONE, KeyCode::Enter) => {
            if !app.search_results.is_empty() {
                app.search_result_index = (app.search_result_index + 1) % app.search_results.len();
                // Scroll chat to the matching message
                let target_msg_idx = app.search_results[app.search_result_index];
                // Approximate scroll: each message is ~3 lines, scroll to show the target
                app.chat_scroll_y = target_msg_idx * 3;
                app.user_scrolled = true;
            }
        }

        // Backspace: delete last char from search query
        (KeyModifiers::NONE, KeyCode::Backspace) => {
            app.search_query.pop();
            update_search_results(app);
        }

        // Ctrl+C: exit search mode
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.search_mode = false;
            app.search_query.clear();
            app.search_results.clear();
            app.search_result_index = 0;
        }

        // Char input: add to search query
        (KeyModifiers::NONE, KeyCode::Char(c)) => {
            app.search_query.push(c);
            update_search_results(app);
        }

        _ => {}
    }
}

/// Update search results based on current search query.
fn update_search_results(app: &mut App) {
    app.search_results.clear();
    app.search_result_index = 0;

    if app.search_query.is_empty() {
        return;
    }

    let query_lower = app.search_query.to_lowercase();
    for (i, msg) in app.messages.iter().enumerate() {
        if msg.content.to_lowercase().contains(&query_lower) {
            app.search_results.push(i);
        }
    }
}

/// Compact conversation — preserve key messages and generate a summary.
///
/// Strategy:
/// 1. Keep the last User message (most recent question)
/// 2. Keep the last Assistant message (most recent answer)
/// 3. Generate a summary line of the conversation context
/// 4. Drop all intermediate tool calls, tool results, iteration markers
/// 5. Reset token/cost counters (new session)
fn compact_conversation(app: &mut App) {
    // Extract key messages to preserve
    let last_user = app.messages.iter().rev()
        .find(|m| m.role == ChatRole::User)
        .map(|m| m.content.clone());

    let last_assistant = app.messages.iter().rev()
        .find(|m| m.role == ChatRole::Assistant)
        .map(|m| m.content.clone());

    // Count message types for summary
    let total_messages = app.messages.len();
    let user_count = app.messages.iter().filter(|m| m.role == ChatRole::User).count();
    let assistant_count = app.messages.iter().filter(|m| m.role == ChatRole::Assistant).count();
    let tool_count = app.messages.iter()
        .filter(|m| matches!(m.role, ChatRole::ToolInvocation { .. }))
        .count();

    // Generate summary
    let ctx_est = if app.context_tokens_is_estimated { "~" } else { "" };
    let summary = format!(
        "📊 Context summary: {} messages ({} user, {} assistant, {} tool calls). Context: {}{} tokens, Cost: ${:.4}",
        total_messages, user_count, assistant_count, tool_count,
        ctx_est, app.context_tokens, app.session_cost
    );

    // Build compacted messages — no longer needed since we directly modify app

    // Add summary as system message
    app.messages.clear();
    app.render_cache.invalidate_all();
    app.add_message(ChatRole::System, summary);

    // Preserve last user message
    if let Some(user_msg) = last_user {
        app.add_message(ChatRole::User, user_msg);
    }

    // Preserve last assistant answer
    if let Some(assistant_msg) = last_assistant {
        app.add_message(ChatRole::Assistant, assistant_msg);
    }

    // Reset counters for new session
    app.token_usage = TokenUsage::new();
    app.context_tokens = 0;
    app.context_tokens_is_estimated = false;
    app.session_cost = 0.0;
    app.current_iteration = 0;
}
