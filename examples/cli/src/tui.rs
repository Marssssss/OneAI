//! TUI (Terminal User Interface) for the OneAI CLI demo.
//!
//! Provides an interactive terminal interface inspired by opencode:
//! - Left sidebar: tools list + session info
//! - Right panel: header bar, scrollable chat area, input box at bottom
//! - Streaming/typewriter effect for assistant responses
//! - Enter = send message, Ctrl+Enter = newline

use std::sync::Arc;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame, Terminal,
};

use oneai_app::AppBuilder;
use oneai_core::ModelConfig;
use oneai_provider::ProviderFactory;
use oneai_tool::CalculatorTool;
use oneai_agent::{AgentLoopObserver, AgentLoopResult, ParadigmKind, ToolCallRequest, SubAgentKind};
use oneai_core::ToolOutput;

// ─── Chat Message ──────────────────────────────────────────────────────────

/// A message in the chat area.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    ToolCall,
    ToolResult,
    Iteration,
    Error,
}

impl ChatRole {
    fn color(&self) -> Color {
        match self {
            ChatRole::User => Color::Cyan,
            ChatRole::Assistant => Color::Green,
            ChatRole::System => Color::Yellow,
            ChatRole::ToolCall => Color::Magenta,
            ChatRole::ToolResult => Color::Blue,
            ChatRole::Iteration => Color::DarkGray,
            ChatRole::Error => Color::Red,
        }
    }

    fn prefix(&self) -> &str {
        match self {
            ChatRole::User => "🤔 ",
            ChatRole::Assistant => "🤖 ",
            ChatRole::System => "⚡ ",
            ChatRole::ToolCall => "🔧 ",
            ChatRole::ToolResult => "✓  ",
            ChatRole::Iteration => "── ",
            ChatRole::Error => "✗  ",
        }
    }
}

// ─── App State ──────────────────────────────────────────────────────────────

/// The TUI application state.
pub struct App {
    /// Whether the app should quit.
    pub should_quit: bool,

    /// Whether the sidebar is visible.
    pub show_sidebar: bool,

    /// Input buffer.
    pub input: String,

    /// Chat messages.
    pub messages: Vec<ChatMessage>,

    /// Scroll position in the chat area.
    pub scroll_offset: u16,

    /// Scrollbar state.
    pub scrollbar_state: ScrollbarState,

    /// Registered tool names.
    pub tool_names: Vec<String>,

    /// Provider info string.
    pub provider_info: String,

    /// Session ID.
    pub session_id: String,

    /// Whether an agent response is in progress.
    pub is_thinking: bool,
}

impl App {
    pub fn new(provider_info: String, tool_names: Vec<String>, session_id: String) -> Self {
        Self {
            should_quit: false,
            show_sidebar: true,
            input: String::new(),
            messages: Vec::new(),
            scroll_offset: 0,
            scrollbar_state: ScrollbarState::default(),
            tool_names,
            provider_info,
            session_id,
            is_thinking: false,
        }
    }

    /// Add a chat message and auto-scroll to bottom.
    pub fn add_message(&mut self, role: ChatRole, content: impl Into<String>) {
        self.messages.push(ChatMessage { role, content: content.into() });
        self.scroll_to_bottom();
    }

    /// Append text to the last assistant message (for streaming/typewriter).
    pub fn append_to_last_assistant(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == ChatRole::Assistant {
                last.content.push_str(text);
                self.scroll_to_bottom();
                return;
            }
        }
        // No assistant message to append to — create one
        self.add_message(ChatRole::Assistant, text.to_string());
    }

    /// Scroll to the bottom of the chat area.
    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0; // Will be recalculated during render
    }

    /// Handle a key event.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> Option<String> {
        // Only handle key press events (ignore release/repeat)
        if key.kind != KeyEventKind::Press {
            return None;
        }

        match (key.modifiers, key.code) {
            // Ctrl+Enter: insert newline
            (KeyModifiers::CONTROL, KeyCode::Enter) => {
                self.input.push('\n');
                None
            }

            // Enter (no modifier): send message
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.input.is_empty() {
                    return None;
                }
                let msg = self.input.clone();
                self.input.clear();
                Some(msg)
            }

            // Escape: quit if input is empty, otherwise clear input
            (_, KeyCode::Esc) => {
                if self.input.is_empty() {
                    self.should_quit = true;
                } else {
                    self.input.clear();
                }
                None
            }

            // Tab: toggle sidebar
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.show_sidebar = !self.show_sidebar;
                None
            }

            // Backspace: delete last char
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.input.pop();
                None
            }

            // Char input
            (KeyModifiers::NONE, KeyCode::Char(c)) => {
                self.input.push(c);
                None
            }

            // Ctrl+C: force quit
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.should_quit = true;
                None
            }

            _ => None,
        }
    }
}

// ─── Rendering ──────────────────────────────────────────────────────────────

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &App) {
    let total_size = f.area();

    // Main layout: sidebar | main panel
    let main_layout = if app.show_sidebar {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(22), Constraint::Min(0)])
            .split(total_size)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(0), Constraint::Min(0)])
            .split(total_size)
    };

    let sidebar_rect = main_layout[0];
    let main_rect = main_layout[1];

    // Main panel: header | chat | input
    let panel_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),   // header
            Constraint::Min(0),      // chat area
            Constraint::Length(3),   // input box
        ])
        .split(main_rect);

    draw_sidebar(f, sidebar_rect, app);
    draw_header(f, panel_layout[0], app);
    draw_chat(f, panel_layout[1], app);
    draw_input(f, panel_layout[2], app);
}

fn draw_sidebar(f: &mut Frame, rect: Rect, app: &App) {
    if rect.width == 0 {
        return;
    }

    let items: Vec<ListItem> = app.tool_names.iter()
        .map(|name| ListItem::new(Line::from(Span::styled(
            format!(" • {}", name),
            Style::default().fg(Color::Green),
        ))))
        .collect();

    let tools_title = Line::from(Span::styled(
        " 🛠 Tools",
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    ));

    let session_title = Line::from(Span::styled(
        " ── Session ──",
        Style::default().fg(Color::DarkGray),
    ));

    let session_info = ListItem::new(Line::from(Span::styled(
        format!(" ● {}", &app.session_id[..8]),
        Style::default().fg(Color::Cyan),
    )));

    let all_items: Vec<ListItem> = vec![
        ListItem::new(tools_title),
    ]
    .into_iter()
    .chain(items)
    .chain(vec![
        ListItem::new(session_title),
        session_info,
    ])
    .collect();

    let sidebar = List::new(all_items)
        .block(Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray))
            .title_style(Style::default().fg(Color::Yellow)));

    f.render_widget(sidebar, rect);
}

fn draw_header(f: &mut Frame, rect: Rect, app: &App) {
    let header_text = if app.is_thinking {
        format!(" OneAI · {} · ⏳ thinking...", app.provider_info)
    } else {
        format!(" OneAI · {} · {}", app.provider_info, &app.session_id[..8])
    };

    let header = Paragraph::new(Line::from(Span::styled(
        header_text,
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));

    f.render_widget(header, rect);
}

fn draw_chat(f: &mut Frame, rect: Rect, app: &App) {
    // Build text lines from messages
    let lines: Vec<Line> = app.messages.iter()
        .flat_map(|msg| {
            let prefix = msg.role.prefix();
            let color = msg.role.color();
            let style = Style::default().fg(color);

            // Split content by newlines, prefix each line
            let content_lines = msg.content.lines().collect::<Vec<&str>>();
            if content_lines.is_empty() {
                vec![Line::from(Span::styled(format!("{}(empty)", prefix), style))]
            } else {
                content_lines.into_iter().enumerate().map(|(i, line)| {
                    if i == 0 {
                        Line::from(Span::styled(format!("{}{}", prefix, line), style))
                    } else {
                        Line::from(Span::styled(format!("   {}", line), style))
                    }
                }).collect()
            }
        })
        .collect();

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .borders(Borders::NONE));

    f.render_widget(paragraph, rect);

    // Render scrollbar
    let content_height = app.messages.iter()
        .map(|msg| msg.content.lines().count().max(1))
        .sum::<usize>() as u16;

    if content_height > rect.height {
        f.render_stateful_widget(
            Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight),
            rect,
            &mut app.scrollbar_state.clone(),
        );
    }
}

fn draw_input(f: &mut Frame, rect: Rect, app: &App) {
    let input_style = Style::default().fg(Color::White);
    let prompt_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);

    let display_text = if app.is_thinking {
        "waiting for response...".to_string()
    } else {
        app.input.clone()
    };

    let input_text = Text::from(vec![
        Line::from(vec![
            Span::styled("oneai> ", prompt_style),
            Span::styled(display_text, input_style),
        ]),
    ]);

    let input_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(input_text)
        .block(input_block);

    f.render_widget(paragraph, rect);
}

// ─── TUI Observer ────────────────────────────────────────────────────────────

/// TUI observer — receives AgentLoop events and updates the App state
/// via a channel, so the TUI can render them in real-time.
pub struct TuiObserver {
    tx: tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
}

/// Events sent from the observer to the TUI event loop.
#[derive(Debug, Clone)]
pub enum ObserverEvent {
    IterationStart(usize, ParadigmKind),
    DirectAnswer(String),
    ToolCalls(Vec<ToolCallRequest>),
    ToolResult(String, String, ToolOutput),
    Delegate(String, SubAgentKind),
    ParadigmSwitch(ParadigmKind),
    Checkpoint(usize),
    Complete(AgentLoopResult),
    #[allow(dead_code)]
    StreamChunk(String),  // text fragment for typewriter effect (used by streaming)
    #[allow(dead_code)]
    Error(String),
}

impl TuiObserver {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ObserverEvent>) -> Self {
        Self { tx }
    }
}

impl AgentLoopObserver for TuiObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        let _ = self.tx.send(ObserverEvent::IterationStart(iteration, paradigm));
    }

    fn on_direct_answer(&self, text: &str) {
        let _ = self.tx.send(ObserverEvent::DirectAnswer(text.to_string()));
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        let _ = self.tx.send(ObserverEvent::ToolCalls(calls.to_vec()));
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput) {
        let _ = self.tx.send(ObserverEvent::ToolResult(call_id.to_string(), tool_name.to_string(), output.clone()));
    }

    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind) {
        let _ = self.tx.send(ObserverEvent::Delegate(task.to_string(), agent_type.clone()));
    }

    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        let _ = self.tx.send(ObserverEvent::ParadigmSwitch(paradigm));
    }

    fn on_checkpoint(&self, iteration: usize) {
        let _ = self.tx.send(ObserverEvent::Checkpoint(iteration));
    }

    fn on_complete(&self, result: &AgentLoopResult) {
        let _ = self.tx.send(ObserverEvent::Complete(result.clone()));
    }

    fn on_stream_chunk(&self, text: &str) {
        let _ = self.tx.send(ObserverEvent::StreamChunk(text.to_string()));
    }
}

// ─── Run TUI ────────────────────────────────────────────────────────────────

/// Run the TUI application.
///
/// Sets up crossterm (raw mode + alternate screen), creates the ratatui
/// terminal, and runs the main event loop.
pub fn run_tui(
    provider_config: Option<ModelConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Setup terminal
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
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
    let (app, session_state) = rt.block_on(async {
        let mut builder = AppBuilder::new()
            .auto_approval_gate()
            .default_parser();

        if let Some(config) = provider_config {
            let provider = ProviderFactory::create(config);
            builder = builder.provider(Arc::from(provider));
        }

        let app = builder.build().await.expect("App build failed");
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();

        let tool_names = app.tool_executor().list_tools().await;
        let session = app.create_session();
        let session_id = session.session_id().to_string();

        let app_arc = Arc::new(app);
        let session_state = SessionState { app: app_arc.clone(), session };
        let session_state = Arc::new(tokio::sync::Mutex::new(session_state));

        (App::new(provider_info, tool_names, session_id), session_state)
    });

    // Channel for observer events
    let (observer_tx, observer_rx) = tokio::sync::mpsc::unbounded_channel();

    // Run the main loop
    let result = run_main_loop(&mut terminal, app, session_state, observer_tx, observer_rx, &rt);

    // Restore terminal
    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Holds shared resources for the TUI session.
struct SessionState {
    app: Arc<oneai_app::App>,
    session: oneai_app::AppSession,
}

impl SessionState {
    #[allow(dead_code)]
    fn new(app: Arc<oneai_app::App>) -> Self {
        let session = app.create_session();
        Self { app, session }
    }

    fn reset_session(&mut self) {
        self.session = self.app.create_session();
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
) -> Result<(), Box<dyn std::error::Error>> {
    while !app.should_quit {
        // Draw current state
        terminal.draw(|f| draw(f, &app))?;

        // Poll for events (crossterm + observer channel)
        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let msg = app.handle_key_event(key);
                if let Some(user_input) = msg {
                    if !app.is_thinking {
                        handle_user_input_async(&mut app, session_state.clone(), user_input, &observer_tx, rt);
                    }
                }
            }
        }

        // Process observer events (this is how streaming/typewriter works)
        while let Ok(event) = observer_rx.try_recv() {
            process_observer_event(&mut app, event);
        }
    }

    Ok(())
}

/// Handle a user input message — send to agent or handle command.
///
/// Commands are handled synchronously. Agent requests are dispatched
/// to a background tokio task, which sends observer events via the
/// channel. The TUI main loop processes these events asynchronously,
/// enabling the typewriter effect.
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
                app.add_message(ChatRole::System, "Commands: /help · /tools · /clear · /quit\nEnter=send, Ctrl+Enter=newline, Tab=sidebar, Esc=quit");
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
                rt.block_on(async {
                    session_state.lock().await.reset_session();
                });
                app.session_id = rt.block_on(async {
                    session_state.lock().await.session.session_id().to_string()
                });
                app.add_message(ChatRole::System, "Conversation cleared.");
                return;
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
                            app.add_message(ChatRole::ToolResult, format!("{}: {}", tool_name, output.content));
                        } else {
                            app.add_message(ChatRole::Error, format!("{}: {}", tool_name, output.error.unwrap_or_default()));
                        }
                    }
                    Err(e) => app.add_message(ChatRole::Error, format!("Error: {e}")),
                }
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

/// Process an observer event and update the app state.
fn process_observer_event(app: &mut App, event: ObserverEvent) {
    match event {
        ObserverEvent::IterationStart(iteration, paradigm) => {
            let paradigm_name = match paradigm {
                ParadigmKind::Plan => "Plan",
                ParadigmKind::ReAct => "ReAct",
                ParadigmKind::Reflect => "Reflect",
                ParadigmKind::Explore => "Explore",
            };
            app.add_message(ChatRole::Iteration, format!("iteration {} ({}) ──", iteration, paradigm_name));
        }
        ObserverEvent::DirectAnswer(text) => {
            // If we already received stream chunks, the text is already displayed.
            // Only add a new message if no streaming chunks were received.
            // The stream chunks accumulate into the last assistant message,
            // and DirectAnswer contains the same text. So we skip it to avoid
            // duplication.
            // Check if the last message already contains this text
            let already_shown = app.messages.last()
                .map(|m| m.role == ChatRole::Assistant && m.content == text)
                .unwrap_or(false);
            if !already_shown {
                app.add_message(ChatRole::Assistant, text);
            }
        }
        ObserverEvent::ToolCalls(calls) => {
            for call in calls {
                app.add_message(ChatRole::ToolCall, format!("calling {}...", call.name));
            }
        }
        ObserverEvent::ToolResult(_call_id, tool_name, output) => {
            if output.success {
                let preview: String = output.content.chars().take(200).collect();
                let more = if output.content.len() > 200 { "..." } else { "" };
                app.add_message(ChatRole::ToolResult, format!("{}: {}{}", tool_name, preview, more));
            } else {
                let err = output.error.as_deref().unwrap_or("unknown error");
                app.add_message(ChatRole::Error, format!("{}: {}", tool_name, err));
            }
        }
        ObserverEvent::Delegate(task, agent_type) => {
            app.add_message(ChatRole::System, format!("delegating to {} sub-agent: {}", agent_type.name(), task));
        }
        ObserverEvent::ParadigmSwitch(paradigm) => {
            let name = match paradigm {
                ParadigmKind::Plan => "Plan",
                ParadigmKind::ReAct => "ReAct",
                ParadigmKind::Reflect => "Reflect",
                ParadigmKind::Explore => "Explore",
            };
            app.add_message(ChatRole::System, format!("switching to {} paradigm", name));
        }
        ObserverEvent::Checkpoint(iteration) => {
            app.add_message(ChatRole::Iteration, format!("checkpoint saved (iteration {})", iteration));
        }
        ObserverEvent::Complete(_result) => {
            // Agent loop finished — stop thinking indicator
            app.is_thinking = false;
        }
        ObserverEvent::StreamChunk(text) => {
            app.append_to_last_assistant(&text);
        }
        ObserverEvent::Error(msg) => {
            app.is_thinking = false;
            app.add_message(ChatRole::Error, msg);
        }
    }
}