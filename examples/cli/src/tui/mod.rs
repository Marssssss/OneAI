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

use app::{App, ApprovalPendingState, ChatRole, TokenUsage, is_file_operation_tool};
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

    // Setup terminal
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

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (app, session_state, approval_rx) = rt.block_on(async {
        // Use ChannelApprovalGateWithThreshold — Medium risk auto-approves,
        // High risk requires human approval via the TUI
        let (builder, approval_rx) = AppBuilder::new()
            .default_parser()
            .channel_approval_gate(16, oneai_core::RiskLevel::Medium);

        let mut builder = builder;
        if let Some(config) = provider_config {
            let provider = ProviderFactory::create(config);
            builder = builder.provider(Arc::from(provider));
        }

        let app = builder.build().await.expect("App build failed");

        // Register domain-specific tools from the coding domain pack
        let domain = coding_pack(".");
        for tool in &domain.tools {
            app.register_tool(tool.clone()).await.unwrap();
        }
        // Also register CalculatorTool as a general-purpose tool
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();

        let tool_names = app.tool_executor().list_tools().await;
        let session = app.create_session();
        let session_id = session.session_id().to_string();

        let app_arc = Arc::new(app);
        let session_state = SessionState { app: app_arc.clone(), session };
        let session_state = Arc::new(tokio::sync::Mutex::new(session_state));

        let mut tui_app = App::new(provider_info, tool_names, session_id);

        // Register built-in skills for the current domain (coding by default)
        let skills = oneai_skill::builtin::skills_for_domain("coding");
        tui_app.skill_registry.register_builtin(skills).await.unwrap();
        tui_app.skill_names = tui_app.skill_registry.skill_names().await;

        (tui_app, session_state, approval_rx)
    });

    // Channel for observer events
    let (observer_tx, observer_rx) = tokio::sync::mpsc::unbounded_channel();

    // Run the main loop
    let result = run_main_loop(&mut terminal, app, session_state, observer_tx, observer_rx, &rt, approval_rx);

    // Restore terminal
    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Handle a single crossterm event (key or mouse).
fn handle_single_event(
    app: &mut App,
    event: Event,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    observer_tx: &tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
    rt: &tokio::runtime::Runtime,
) {
    match event {
        Event::Key(key) => {
            if app.approval_pending.is_some() {
                handle_approval_key(app, key);
                // add_message inside handle_approval_key sets dirty
                return;
            }
            if app.search_mode {
                handle_search_key(app, key);
                app.dirty = true; // search mode always updates UI
                return;
            }
            let msg = app.handle_key_event(key);
            // handle_key_event already sets dirty = true
            if let Some(user_input) = msg {
                if !app.is_thinking {
                    handle_user_input_async(app, session_state.clone(), user_input, observer_tx, rt);
                }
            }
        }
        // Handle mouse scroll events for chat area
        // macOS natural scrolling convention: ScrollUp shows later content (scroll down in viewport),
        // ScrollDown shows earlier content (scroll up in viewport).
        Event::Mouse(mouse_event) => {
            match mouse_event.kind {
                crossterm::event::MouseEventKind::ScrollDown => {
                    // Swipe/scroll down → scroll content up (see earlier content)
                    app.chat_scroll_y = app.chat_scroll_y.saturating_add(3);
                    app.user_scrolled = true;
                    app.dirty = true;
                }
                crossterm::event::MouseEventKind::ScrollUp => {
                    // Swipe/scroll up → scroll content down (see later content)
                    app.chat_scroll_y = app.chat_scroll_y.saturating_sub(3);
                    app.user_scrolled = true;
                    app.dirty = true;
                }
                // Scrollbar drag support — map drag Y position to scroll position
                crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                    let chat_rect = app.last_chat_rect;
                    // Only handle drags in the scrollbar column (rightmost 2 columns of chat area)
                    if chat_rect.width > 0
                        && mouse_event.column >= chat_rect.x + chat_rect.width - 2
                        && mouse_event.column <= chat_rect.x + chat_rect.width
                        && mouse_event.row >= chat_rect.y
                        && mouse_event.row < chat_rect.y + chat_rect.height {
                        // Map drag Y to scroll position proportionally
                        let y_offset = (mouse_event.row - chat_rect.y) as usize;
                        let track_height = chat_rect.height as usize;
                        if track_height > 0 {
                            // Calculate content_height from the render cache's known line count
                            // We use a rough estimate based on messages, or just use ratio
                            // The exact content height is computed during render — we store it
                            let content_height = app.content_height;
                            let viewport_height = chat_rect.height as usize;
                            if content_height > viewport_height {
                                let ratio = y_offset as f64 / track_height as f64;
                                app.chat_scroll_y = ((ratio * content_height as f64) as usize)
                                    .min(content_height.saturating_sub(viewport_height));
                                app.user_scrolled = true;
                                app.dirty = true;
                            }
                        }
                    }
                }
                // Click on scrollbar track to jump to position
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                    let chat_rect = app.last_chat_rect;
                    // Only handle clicks in the scrollbar column
                    if chat_rect.width > 0
                        && mouse_event.column >= chat_rect.x + chat_rect.width - 2
                        && mouse_event.column <= chat_rect.x + chat_rect.width
                        && mouse_event.row >= chat_rect.y
                        && mouse_event.row < chat_rect.y + chat_rect.height {
                        let y_offset = (mouse_event.row - chat_rect.y) as usize;
                        let track_height = chat_rect.height as usize;
                        if track_height > 0 {
                            let content_height = app.content_height;
                            let viewport_height = chat_rect.height as usize;
                            if content_height > viewport_height {
                                let ratio = y_offset as f64 / track_height as f64;
                                app.chat_scroll_y = ((ratio * content_height as f64) as usize)
                                    .min(content_height.saturating_sub(viewport_height));
                                app.user_scrolled = true;
                                app.dirty = true;
                            }
                        }
                    }
                }
                _ => {}
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
            handle_single_event(&mut app, first_event, session_state.clone(), &observer_tx, rt);

            // Drain all pending events (especially scroll events) in one frame
            // for responsive continuous scrolling
            while event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    handle_single_event(&mut app, ev, session_state.clone(), &observer_tx, rt);
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

            // Check if the tool is in the session allowlist
            if app.session_allowlist.contains(&tool_name) {
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
    }
}

fn handle_user_input_async(
    app: &mut App,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    input: String,
    observer_tx: &tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
    rt: &tokio::runtime::Runtime,
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
                app.add_message(ChatRole::System, "Commands:\n  /help · /tools · /skills · /skill · /clear · /cost · /session · /paradigm · /domain · /compact · /wf · /tool · /new · /quit\nKeys: Enter=send, Ctrl+Enter=newline, Tab=sidebar, Esc=vim/quit, ↑↓=history\nSkills: /skill <name> activate · /skill off deactivate · /skill add <name> <desc>\nWorkflows: /wf list · /wf run <name> · /wf show <name> · /wf graph <name> · /wf status · /wf history");
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
                app.session_cost = 0.0;
                app.session_cost_is_estimated = false;
                app.current_iteration = 0;
                // Create a new session entry in the sidebar
                app.add_new_session(app.session_id.clone());
                app.add_message(ChatRole::System, "Conversation cleared.");
                return;
            }
            "/cost" => {
                let tokens = app.token_usage.format_display();
                app.add_message(ChatRole::System, format!(
                    "Session cost: ${:.4}\nTokens: {} (prompt: {}, completion: {})",
                    app.session_cost, tokens, app.token_usage.prompt, app.token_usage.completion
                ));
                return;
            }
            "/session" => {
                app.add_message(ChatRole::System, format!(
                    "Session ID: {}\nProvider: {}\nParadigm: {}#{}\nTokens: {}\nCost: ${:.4}",
                    app.session_id,
                    app.provider_info,
                    paradigm_display_name(&app.active_paradigm),
                    app.current_iteration,
                    app.token_usage.format_display(),
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
                        "Current domain: {}\nAvailable domains:\n  • coding — Software development (8 tools: read, edit, grep, glob, shell, list, notebook, environment)\n  • general — General-purpose (calculator only)\n\nUsage: /domain <name>",
                        app.current_domain,
                    ));
                    return;
                }
                let name = parts[1];
                match name {
                    "coding" => {
                        // Register coding domain tools
                        rt.block_on(async {
                            let domain = coding_pack(".");
                            let state = session_state.lock().await;
                            for tool in &domain.tools {
                                state.app.register_tool(tool.clone()).await.unwrap();
                            }
                            // Also keep CalculatorTool
                            state.app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
                            app.tool_names = state.app.tool_executor().list_tools().await;
                            // Switch skills to coding domain
                            app.skill_registry.replace_all(oneai_skill::builtin::skills_for_domain("coding")).await.unwrap();
                            app.skill_names = app.skill_registry.skill_names().await;
                        });
                        app.current_domain = "coding".to_string();
                        app.active_skill = None;
                        app.add_message(ChatRole::System, "Switched to coding domain. Tools: read_file, edit_file, shell, grep, glob, list_directory, notebook_edit, environment, calculator\nSkills: project-planning, code-review, debug-analysis, refactoring, test-strategy, documentation, git-workflow, dependency-analysis + general skills");
                    }
                    "general" => {
                        // Minimal domain — just calculator (note: cannot remove existing tools dynamically)
                        rt.block_on(async {
                            // Switch skills to general-only (no domain-specific skills)
                            app.skill_registry.replace_all(oneai_skill::builtin::skills_for_domain("general")).await.unwrap();
                            app.skill_names = app.skill_registry.skill_names().await;
                        });
                        app.current_domain = "general".to_string();
                        app.active_skill = None;
                        app.add_message(ChatRole::System, "Switched to general domain. Note: existing tools remain registered until session restart.\nSkills: summarization, translation, creative-writing");
                    }
                    _ => {
                        app.add_message(ChatRole::Error, format!("Unknown domain: {}. Available: coding, general", name));
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
                app.session_cost = 0.0;
                app.session_cost_is_estimated = false;
                app.current_iteration = 0;
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
                        rt.block_on(async {
                            let skill = app.skill_registry.find_by_name(skill_name).await;
                            if let Some(s) = skill {
                                app.active_skill = Some(s.name.clone());
                                app.add_message(ChatRole::System, format!(
                                    "✅ Skill activated: {}\nPrompt injected: \"{}\"\nThe agent will now prioritize this skill's approach.\nType /skill off to deactivate, or /skill <other> to switch.",
                                    s.name, s.prompt_template
                                ));
                            } else {
                                app.add_message(ChatRole::Error, format!("Skill '{}' not found. Use /skills to see available skills.", skill_name));
                            }
                        });
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
                            app.add_message(ChatRole::ToolResult {
                                call_id: String::new(),
                                success: true,
                                tool_name: tool_name.to_string(),
                            }, format!("{}: {}", tool_name, output.content));
                        } else {
                            app.add_message(ChatRole::ToolResult {
                                call_id: String::new(),
                                success: false,
                                tool_name: tool_name.to_string(),
                            }, format!("{}: {}", tool_name, output.error.unwrap_or_default()));
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
    app.is_thinking = true;
    // Add a thinking bubble to show the agent is processing
    app.add_collapsed_message(ChatRole::Thinking, "Processing your request...");

    let task = trimmed.to_string();
    let tx1 = observer_tx.clone();
    let tx2 = observer_tx.clone();

    rt.spawn(async move {
        let mut state = session_state.lock().await;
        let observer = TuiObserver::new(tx1);

        let result = state.session.run_agent(&task, &observer).await;

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
                app.add_collapsed_message(
                    ChatRole::ToolCall {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        args: args_str,
                    },
                    format!("calling {}...", call.name),
                );
            }
        }
        ObserverEvent::ToolResult(_call_id, tool_name, output) => {
            if output.success {
                // Smart display based on tool type and output content:
                // - File operations (read/write/edit): always show content with 3-line preview
                // - Empty output (mkdir, etc.) → show success indicator for file ops, skip for others
                // - Short output (pwd, ls, etc.) → show inline
                // - Long output → show collapsed with preview
                let is_file_op = is_file_operation_tool(&tool_name);
                let content_trimmed = output.content.trim();

                if is_file_op {
                    // File operations: always show a result card
                    // - For read_file: show file content with 3-line preview
                    // - For write/edit: show success + content preview from the output
                    // Even if output is empty (e.g., write succeeded), show what was written
                    if content_trimmed.is_empty() {
                        // Write/edit succeeded — show success message with hint
                        app.add_collapsed_message(
                            ChatRole::ToolResult {
                                call_id: _call_id,
                                success: true,
                                tool_name: tool_name.clone(),
                            },
                            format!("{} completed successfully", tool_name),
                        );
                    } else {
                        // Read or edit with output — show content with 3-line preview
                        let preview_lines: Vec<&str> = content_trimmed.lines().take(3).collect();
                        let preview = preview_lines.join("\n");
                        let total_lines = content_trimmed.lines().count();
                        let preview_text = if total_lines > 3 {
                            format!("{}: {} ▸({} more lines)", tool_name, preview, total_lines - 3)
                        } else {
                            format!("{}: {}", tool_name, preview)
                        };
                        app.add_collapsed_message(
                            ChatRole::ToolResult {
                                call_id: _call_id,
                                success: true,
                                tool_name: tool_name.clone(),
                            },
                            preview_text,
                        );
                        // Store full content in the message for expansion
                        // The full content is already in output.content
                        // We need to replace the preview with the full content when expanded
                        if let Some(last_msg) = app.messages.last_mut() {
                            // Store full content separately — we'll use it in rendering
                            // The content field will hold the preview, and we need to store full content
                            // Actually, let's store the full content in the message content
                            // and handle the preview rendering in the renderer
                            last_msg.content = content_trimmed.to_string();
                        }
                        // Invalidate cache since we changed content
                        if let Some(last_msg) = app.messages.last() {
                            app.render_cache.invalidate(&last_msg.id);
                        }
                    }
                } else if content_trimmed.is_empty() {
                    // Non-file tool with empty output — silently skip
                } else if output.content.len() > 200 {
                    // Long output — collapsed with preview
                    let preview: String = content_trimmed.chars().take(150).collect();
                    app.add_collapsed_message(
                        ChatRole::ToolResult {
                            call_id: _call_id,
                            success: true,
                            tool_name: tool_name.clone(),
                        },
                        format!("{}: {}…", tool_name, preview),
                    );
                } else {
                    // Short output — show inline
                    app.add_message(
                        ChatRole::ToolResult {
                            call_id: _call_id,
                            success: true,
                            tool_name: tool_name.clone(),
                        },
                        format!("{}: {}", tool_name, content_trimmed),
                    );
                }
            } else {
                let err = output.error.as_deref().unwrap_or("unknown error");
                app.add_message(
                    ChatRole::ToolResult {
                        call_id: _call_id,
                        success: false,
                        tool_name: tool_name.clone(),
                    },
                    format!("{}: {}", tool_name, err),
                );
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
            };
            app.add_message(ChatRole::System, format!("switching to {} paradigm", name));
        }
        ObserverEvent::Checkpoint(_iteration) => {
            // No visible message — checkpoint is silent in TUI
        }
        ObserverEvent::Complete(_result) => {
            // Flush any remaining buffered stream text before marking as done
            app.flush_stream_buffer();
            app.is_thinking = false;
            // Remove placeholder thinking bubble if no real thinking content was received
            // (model didn't use extended thinking, so the bubble is useless)
            app.messages.retain(|m| {
                if m.role == ChatRole::Thinking && m.content == "Processing your request..." {
                    false // remove placeholder
                } else {
                    true
                }
            });
            app.dirty = true; // Need redraw to stop spinner
        }
        ObserverEvent::StreamChunk(text) => {
            app.append_to_last_assistant(&text);
        }
        ObserverEvent::Thinking(text) => {
            // Find the last Thinking message and append/replace content
            if let Some(thinking_msg) = app.messages.iter_mut().rev()
                .find(|m| m.role == ChatRole::Thinking)
            {
                if thinking_msg.content == "Processing your request..." {
                    // Replace placeholder with real thinking content
                    thinking_msg.content = text.clone();
                } else {
                    // Append thinking fragment (streamed in chunks)
                    thinking_msg.content.push_str(&text);
                }
                // Auto-expand thinking bubble when it has real content so user can see it
                app.collapsed_ids.remove(&thinking_msg.id);
            } else {
                // No existing thinking bubble — create one with this content
                app.add_message(ChatRole::Thinking, text.clone());
            }
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
            // Accumulate token usage across iterations
            app.token_usage.prompt += usage.prompt;
            app.token_usage.completion += usage.completion;
            app.token_usage.total += usage.total;
            // If provider returns zero tokens, estimate from conversation content
            if app.token_usage.prompt == 0 && app.token_usage.completion == 0 && !app.messages.is_empty() {
                let estimated = app.estimate_tokens_from_messages();
                if estimated > 0 {
                    app.token_usage.prompt = estimated * 3 / 4; // ~75% are prompt tokens
                    app.token_usage.completion = estimated / 4;  // ~25% are completion tokens
                    app.token_usage.total = estimated;
                    app.token_usage.is_estimated = true;
                    app.session_cost = App::estimate_cost_from_tokens(
                        app.token_usage.prompt, app.token_usage.completion
                    );
                    app.session_cost_is_estimated = true;
                }
            } else if app.token_usage.prompt > 0 || app.token_usage.completion > 0 {
                // Got real token data — mark as not estimated
                app.token_usage.is_estimated = false;
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
        .filter(|m| matches!(m.role, ChatRole::ToolCall { .. } | ChatRole::ToolResult { .. }))
        .count();

    // Generate summary
    let summary = format!(
        "📊 Context summary: {} messages ({} user, {} assistant, {} tool calls). Tokens: {}, Cost: ${:.4}",
        total_messages, user_count, assistant_count, tool_count,
        app.token_usage.format_display(), app.session_cost
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
    app.session_cost = 0.0;
    app.current_iteration = 0;
}
