//! App state structure and event handling for the OneAI TUI.
//!
//! Contains the main App state, ChatMessage/ChatRole definitions,
//! and key event handling logic.

use std::collections::HashMap;
use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::ScrollbarState;

use oneai_agent::ParadigmKind;
use oneai_core::ApprovalRequest;

use oneai_skill::SkillRegistry;

use super::input_mode::{InputMode, VimMode};
use super::history::MessageHistory;

// ─── Slash Commands ───────────────────────────────────────────────────────

/// Supported slash commands for autocomplete.
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help",    "Show help and available commands"),
    ("/h",       "Shortcut for /help"),
    ("/tools",   "List registered tools"),
    ("/t",       "Shortcut for /tools"),
    ("/skills",  "List all available skills"),
    ("/skill",   "Activate, add, remove, or search skills (use /skill <name>)"),
    ("/clear",   "Clear conversation and create new session"),
    ("/cost",    "Show session cost and context usage"),
    ("/context", "Show detailed context window usage breakdown"),
    ("/session", "Show session details"),
    ("/paradigm", "Switch agent paradigm (ReAct/Plan/Reflect/Explore)"),
    ("/domain",  "Switch domain pack (coding/general)"),
    ("/compact", "Compact conversation context"),
    ("/wf",      "Workflow commands: list, run, define, show, graph"),
    ("/new",     "Create a new session"),
    ("/tool",    "Directly call a tool with JSON args"),
    ("/quit",    "Exit the TUI"),
    ("/q",       "Shortcut for /quit"),
];

impl App {
    /// Get filtered command suggestions based on current input.
    pub fn get_command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        let prefix = &self.input;
        SLASH_COMMANDS.iter()
            .filter(|(cmd, _)| cmd.starts_with(prefix))
            .map(|&(cmd, desc)| (cmd, desc))
            .collect()
    }

    /// Accept the currently selected autocomplete suggestion.
    pub fn accept_autocomplete(&mut self) {
        let suggestions = self.get_command_suggestions();
        if suggestions.is_empty() {
            return;
        }
        let idx = self.command_autocomplete_index.min(suggestions.len() - 1);
        self.input = suggestions[idx].0.to_string();
        self.input_cursor_pos = self.input.len();
        self.command_autocomplete = false;
        self.command_autocomplete_index = 0;
    }
}

/// Check if a tool name is a file operation tool that should display content.
pub fn is_file_operation_tool(tool_name: &str) -> bool {
    matches!(tool_name,
        "read_file" | "file_read" | "read" |
        "edit_file" | "file_edit" | "edit" |
        "file_write" | "write" |
        "notebook_edit" |
        "list_directory" | "ls"
    )
}// ─── Token Usage ──────────────────────────────────────────────────────────

/// Token usage tracking for the session.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub total: u32,
    /// Whether these values are estimated (from character count) rather than actual API-reported.
    pub is_estimated: bool,
}

impl TokenUsage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Format token count for display (e.g., "1.2k" for 1200).
    /// If estimated, prefix with ~ (e.g., "~1.2k").
    pub fn format_display(&self) -> String {
        let count_str = if self.total >= 1000 {
            format!("{:.1}k", self.total as f64 / 1000.0)
        } else {
            format!("{}", self.total)
        };
        if self.is_estimated {
            format!("~{}", count_str)
        } else {
            count_str
        }
    }
}

// ─── Approval Pending State ───────────────────────────────────────────────

/// State for a pending approval request in the TUI.
#[derive(Debug)]
pub struct ApprovalPendingState {
    pub request: ApprovalRequest,
    pub tool_name: String,
    pub justification: String,
    /// The oneshot channel to send the approval response back.
    /// This is optional because it gets consumed when the user responds.
    pub response_tx: Option<tokio::sync::oneshot::Sender<oneai_core::ApprovalResponse>>,
}

// ─── Chat Message ──────────────────────────────────────────────────────────

/// A message in the chat area.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChatMessage {
    /// Unique message ID (for collapse state management).
    pub id: String,
    /// The role/type of this message.
    pub role: ChatRole,
    /// The content text.
    pub content: String,
    /// Timestamp of the message.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Whether this message is collapsed (tool cards, long results).
    pub collapsed: bool,
    /// Token usage for this turn (if applicable).
    pub token_usage: Option<TokenUsage>,
    /// The paradigm that was active when this message was created.
    pub paradigm: Option<ParadigmKind>,
    /// The iteration number when this message was created.
    pub iteration: Option<usize>,
}

impl ChatMessage {
    /// Create a new chat message with auto-generated ID and timestamp.
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            content: content.into(),
            timestamp: chrono::Utc::now(),
            collapsed: false,
            token_usage: None,
            paradigm: None,
            iteration: None,
        }
    }

    /// Create a collapsed message (for tool cards, long results).
    pub fn new_collapsed(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            content: content.into(),
            timestamp: chrono::Utc::now(),
            collapsed: true,
            token_usage: None,
            paradigm: None,
            iteration: None,
        }
    }
}

// ─── Chat Role ──────────────────────────────────────────────────────────────

/// The role/type of a chat message.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    /// A unified tool invocation message — merges ToolCall + ToolResult into
    /// a single message. When the tool call starts, `result` is None.
    /// When the result arrives, `result` is set with success + output content.
    /// This eliminates the "two cards for one action" duplication problem.
    ToolInvocation {
        call_id: String,
        tool_name: String,
        args: String,
        /// The tool execution result — None when call is pending,
        /// Some((success, output_content)) when result arrives.
        result: Option<(bool, String)>,
    },
    Iteration,
    Error,
    Approval,
    Thinking,
}

impl ChatRole {
    /// Get the display color for this role.
    #[allow(dead_code)]
    pub fn color(&self) -> ratatui::style::Color {
        use super::theme::*;
        match self {
            ChatRole::User => USER_COLOR,
            ChatRole::Assistant => ASSISTANT_COLOR,
            ChatRole::System => SYSTEM_COLOR,
            ChatRole::ToolInvocation { result, .. } => {
                match result {
                    Some((success, _)) => {
                        if *success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR }
                    }
                    None => TOOL_CALL_COLOR,
                }
            }
            ChatRole::Iteration => ratatui::style::Color::DarkGray,
            ChatRole::Error => ERROR_COLOR,
            ChatRole::Approval => APPROVAL_COLOR,
            ChatRole::Thinking => THINKING_COLOR,
        }
    }

    /// Get the border color for this role's bubble/card.
    #[allow(dead_code)]
    pub fn border_color(&self) -> ratatui::style::Color {
        use super::theme::*;
        match self {
            ChatRole::User => USER_BORDER,
            ChatRole::Assistant => ASSISTANT_BORDER,
            ChatRole::System => ratatui::style::Color::DarkGray,
            ChatRole::ToolInvocation { result, .. } => {
                match result {
                    Some((success, _)) => {
                        if *success { TOOL_RESULT_SUCCESS_COLOR } else { TOOL_RESULT_FAILURE_COLOR }
                    }
                    None => TOOL_CALL_BORDER,
                }
            }
            ChatRole::Iteration => ratatui::style::Color::DarkGray,
            ChatRole::Error => ERROR_COLOR,
            ChatRole::Approval => APPROVAL_BORDER,
            ChatRole::Thinking => ratatui::style::Color::DarkGray,
        }
    }

    /// Get the icon/prefix for this role.
    #[allow(dead_code)]
    pub fn icon(&self) -> &str {
        match self {
            ChatRole::User => "💬",
            ChatRole::Assistant => "🤖",
            ChatRole::System => "⚡",
            ChatRole::ToolInvocation { tool_name, result, .. } => {
                // When result is pending, show tool-specific call icon
                // When result arrived, show success/failure icon
                match result {
                    Some((success, _)) => {
                        if *success { "✅" } else { "❌" }
                    }
                    None => {
                        match tool_name.as_str() {
                            "calculator" => "🧮",
                            "grep" | "search" => "🔍",
                            "edit_file" | "file_edit" => "✏️",
                            "read_file" | "file_read" => "📄",
                            "glob" | "file_glob" => "📂",
                            "shell" => "🖥️",
                            "list_directory" => "📂",
                            "web_fetch" => "🌐",
                            _ => "🔧",
                        }
                    }
                }
            }
            ChatRole::Iteration => "──",
            ChatRole::Error => "✗",
            ChatRole::Approval => "⚠️",
            ChatRole::Thinking => "⏳",
        }
    }

    /// Get the label/title for this role's bubble.
    #[allow(dead_code)]
    pub fn label(&self) -> &str {
        match self {
            ChatRole::User => "User",
            ChatRole::Assistant => "Assistant",
            ChatRole::System => "System",
            ChatRole::ToolInvocation { tool_name, .. } => tool_name.as_str(),
            ChatRole::Iteration => "Iteration",
            ChatRole::Error => "Error",
            ChatRole::Approval => "Approval Required",
            ChatRole::Thinking => "Thinking",
        }
    }

    /// Whether this role type should default to collapsed.
    /// Tool invocations are collapsed ONLY when the result is long (>500 chars)
    /// or when still pending (no result yet).
    pub fn default_collapsed(&self) -> bool {
        match self {
            ChatRole::ToolInvocation { result, .. } => {
                match result {
                    None => true, // Pending tool call — collapsed while executing
                    Some((_, content)) => content.len() > 500, // Only collapse long results
                }
            }
            ChatRole::Thinking => true,
            _ => false,
        }
    }
}

// ─── Session Info ──────────────────────────────────────────────────────────

/// Lightweight session descriptor for sidebar display.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionInfo {
    /// Short session ID (first 8 chars).
    pub short_id: String,
    /// Full session ID.
    pub full_id: String,
    /// Number of messages in this session.
    pub message_count: usize,
    /// Whether this is the currently active session.
    pub is_active: bool,
    /// Preview of the first user message (truncated).
    pub preview: String,
}

// ─── Render Cache ──────────────────────────────────────────────────────────

/// A cached rendered message — avoids re-parsing markdown/syntect every frame.
pub struct CachedMessage {
    /// The pre-rendered lines for this message.
    pub lines: Vec<Line<'static>>,
    /// Hash of the content string when this cache entry was created.
    /// Used to detect when the message content has changed (e.g., streaming append).
    pub content_hash: u64,
    /// Whether this was rendered in collapsed state.
    pub was_collapsed: bool,
}

/// Render cache for all messages, keyed by message ID.
///
/// On each frame, only messages that have changed (new content, changed collapsed
/// state, or width change) need to be re-rendered. All others can use cached lines.
pub struct MessageRenderCache {
    /// Cached rendered lines for each message, keyed by message ID.
    pub entries: HashMap<String, CachedMessage>,
    /// The viewport width used for the last render cycle.
    /// When the width changes (terminal resize), all cache entries are invalidated.
    last_width: usize,
}

impl MessageRenderCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_width: 0,
        }
    }

    /// Invalidate a specific message's cache entry (e.g., after streaming append).
    pub fn invalidate(&mut self, id: &str) {
        self.entries.remove(id);
    }

    /// Invalidate all cache entries (e.g., on terminal resize or /clear).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    /// Check if the cache needs full invalidation due to width change.
    /// Returns true if the width changed, and clears all entries.
    pub fn check_width_change(&mut self, new_width: usize) -> bool {
        if self.last_width != new_width {
            self.entries.clear();
            self.last_width = new_width;
            true
        } else {
            false
        }
    }
}

/// Simple content hash using the string's byte representation.
pub fn content_hash(s: &str) -> u64 {
    // Use a simple hash — we just need to detect content changes,
    // not cryptographic security. The hash is based on length + first/last bytes.
    if s.is_empty() {
        return 0;
    }
    let len = s.len() as u64;
    let first = s.as_bytes()[0] as u64;
    let last = s.as_bytes()[s.len() - 1] as u64;
    len ^ (first << 8) ^ (last << 16)
}

// ─── App State ──────────────────────────────────────────────────────────────

/// The TUI application state.
pub struct App {
    /// Whether the app should quit.
    pub should_quit: bool,

    /// Whether the UI needs to be redrawn (dirty flag for conditional rendering).
    /// Set to true on any state change; reset to false after drawing.
    pub dirty: bool,

    /// Whether the sidebar is visible.
    pub show_sidebar: bool,

    /// Input buffer.
    pub input: String,

    /// Cursor position in the input buffer (single-line mode).
    /// 0 = before first char, input.len() = after last char.
    pub input_cursor_pos: usize,

    /// Chat messages.
    pub messages: Vec<ChatMessage>,

    /// Scrollbar state.
    pub scrollbar_state: ScrollbarState,

    /// Registered tool names.
    pub tool_names: Vec<String>,

    /// Skill registry (manages all registered skills).
    pub skill_registry: SkillRegistry,

    /// Skill names for sidebar display (sorted alphabetically).
    /// Updated whenever skills are added/removed/switched.
    pub skill_names: Vec<String>,

    /// Currently activated skill name (None = no active skill).
    /// When a skill is active, its prompt_template is injected into
    /// the agent's system prompt for every query.
    pub active_skill: Option<String>,

    /// Provider info string (e.g., "阿里百炼 · qwen-plus").
    pub provider_info: String,

    /// Raw model name for token counting (e.g., "qwen-plus").
    /// Used by ContextAccounting to pick the right tokenizer profile.
    pub model_name: String,

    /// Session ID.
    pub session_id: String,

    /// Current active domain name (e.g., "coding", "research").
    pub current_domain: String,

    /// Model context window size in tokens (default: 128000).
    pub context_window_size: u32,

    /// All sessions (for sidebar display and switching).
    pub sessions: Vec<SessionInfo>,

    /// Whether an agent response is in progress.
    pub is_thinking: bool,

    // ─── Enhanced fields ──────────────────────────────────────────────────

    /// Current input mode (single-line or multi-line vim).
    pub input_mode: InputMode,

    /// Current active paradigm.
    pub active_paradigm: ParadigmKind,

    /// Current iteration number.
    pub current_iteration: usize,

    /// Cumulative token usage (prompt + completion across all iterations).
    pub token_usage: TokenUsage,

    /// Current context size — prompt tokens from the **latest** inference iteration.
    /// This represents the actual context window occupancy (not cumulative).
    /// Updated each iteration; if the provider returns 0, estimated from messages.
    pub context_tokens: u32,

    /// Whether context_tokens is estimated (from character count) rather than API-reported.
    pub context_tokens_is_estimated: bool,

    /// Whether session_cost is estimated (from token estimation) rather than API-reported.
    pub session_cost_is_estimated: bool,

    /// Cumulative session cost.
    pub session_cost: f64,

    /// IDs of messages that are currently collapsed.
    pub collapsed_ids: HashSet<String>,

    /// Message history for ↑↓ navigation.
    pub message_history: MessageHistory,

    /// Pending approval request (if any).
    pub approval_pending: Option<ApprovalPendingState>,

    /// Selected option index in approval UI (0=Y, 1=N, 2=M, 3=A).
    pub approval_selected_index: usize,

    /// Session-level approval allowlist (tool names auto-approved).
    pub session_allowlist: HashSet<String>,

    /// Vim mode (for multi-line input).
    #[allow(dead_code)]
    pub vim_mode: VimMode,

    /// Spinner animation frame counter.
    pub spinner_frame: usize,

    /// Chat area scroll position: number of lines scrolled from the top.
    /// 0 = top of content, max = content_height - viewport_height (bottom).
    pub chat_scroll_y: usize,

    /// Whether the user has manually scrolled up (disabling auto-scroll-to-bottom).
    /// Reset to false on new user messages to re-enable auto-scroll.
    /// During streaming, remains true if user scrolled up, so they can read earlier content.
    pub user_scrolled: bool,

    /// Last known chat area rect dimensions (for scrollbar drag coordinate mapping).
    pub last_chat_rect: ratatui::layout::Rect,

    /// Total content height in lines (computed during render, used for scrollbar drag).
    pub content_height: usize,

    /// Pending vim command (e.g., 'd' waiting for second 'd' to form 'dd').
    pub vim_pending_cmd: Option<char>,

    /// Input undo history (for Ctrl+Z). Stores previous input states.
    pub input_undo_stack: Vec<String>,

    /// Whether search mode is active (Ctrl+F).
    pub search_mode: bool,

    /// Whether slash command autocomplete is active (user typed /).
    pub command_autocomplete: bool,

    /// Selected command in autocomplete list (0-based index).
    pub command_autocomplete_index: usize,

    /// Current search query string.
    pub search_query: String,

    /// Indices of messages matching the search query.
    pub search_results: Vec<usize>,

    /// Current highlighted search result index.
    pub search_result_index: usize,

    // ─── Stream throttle ──────────────────────────────────────────────────

    /// Buffered stream text not yet applied to the last assistant message.
    /// During streaming, chunks are buffered and flushed at ~10fps for smoother rendering.
    pub stream_buffer: String,

    /// Timestamp of last stream buffer flush (for throttle timing).
    pub last_stream_flush: std::time::Instant,

    // ─── Render cache ─────────────────────────────────────────────────────

    /// Cached rendered lines per message (avoids re-parsing markdown every frame).
    pub render_cache: MessageRenderCache,

    /// Latest context accounting from the assembled inference request.
    /// Updated each iteration by `ContextAccountingUpdate` from the AgentLoop.
    /// The `/context` command reads this instead of recomputing from bare
    /// session conversation — it reflects the actual assembled context that
    /// the model sees (system prompt, tool defs, domain pack, etc.), not
    /// just the bare conversation messages.
    /// None until the first agent iteration completes.
    pub last_context_accounting: Option<oneai_core::ContextAccounting>,
}

impl App {
    pub fn new(provider_info: String, model_name: String, tool_names: Vec<String>, session_id: String) -> Self {
        // Compute context window from model name before it's moved into the struct
        let context_window_size = oneai_core::token_counter::infer_context_window_for_tokenizer(model_name.as_str());
        let short_id = session_id[..8.min(session_id.len())].to_string();
        let initial_session = SessionInfo {
            short_id,
            full_id: session_id.clone(),
            message_count: 0,
            is_active: true,
            preview: String::new(),
        };

        Self {
            should_quit: false,
            dirty: true, // First frame must always draw
            show_sidebar: true,
            input: String::new(),
            input_cursor_pos: 0,
            messages: Vec::new(),
            scrollbar_state: ScrollbarState::default(),
            tool_names,
            skill_registry: SkillRegistry::new(),
            skill_names: Vec::new(),
            active_skill: None,
            provider_info,
            model_name,
            session_id,
            sessions: vec![initial_session],
            current_domain: "coding".to_string(),
            context_window_size,
            is_thinking: false,

            input_mode: InputMode::default(),
            active_paradigm: ParadigmKind::ReAct,
            current_iteration: 0,
            token_usage: TokenUsage::new(),
            context_tokens: 0,
            context_tokens_is_estimated: false,
            session_cost: 0.0,
            session_cost_is_estimated: false,
            collapsed_ids: HashSet::new(),
            message_history: MessageHistory::new(),
            approval_pending: None,
            approval_selected_index: 0,
            session_allowlist: HashSet::new(),
            vim_mode: VimMode::default(),
            spinner_frame: 0,
            chat_scroll_y: 0,
            user_scrolled: false,
            last_chat_rect: ratatui::layout::Rect::default(),
            content_height: 0,
            vim_pending_cmd: None,
            input_undo_stack: Vec::new(),
            search_mode: false,
            command_autocomplete: false,
            command_autocomplete_index: 0,
            search_query: String::new(),
            search_results: Vec::new(),
            search_result_index: 0,
            stream_buffer: String::new(),
            last_stream_flush: std::time::Instant::now(),
            render_cache: MessageRenderCache::new(),
            last_context_accounting: None,
        }
    }

    /// Estimate token count from conversation content when provider returns 0.
    /// Approximate: ~4 characters = 1 token (for English/mixed text).
    pub fn estimate_tokens_from_messages(&self) -> u32 {
        let total_chars: usize = self.messages.iter()
            .map(|m| m.content.len())
            .sum();
        (total_chars / 4) as u32
    }

    /// Estimate cost from estimated token count.
    /// Uses rough pricing: $0.003/1k prompt tokens, $0.015/1k completion tokens.
    pub fn estimate_cost_from_tokens(prompt: u32, completion: u32) -> f64 {
        (prompt as f64 / 1000.0) * 0.003 + (completion as f64 / 1000.0) * 0.015
    }

    /// Add a chat message and auto-scroll to bottom.
    ///
    /// For User messages, this resets user_scrolled to re-enable auto-scroll.
    /// For other message types (system, tool, etc.), auto-scroll behavior is preserved.
    pub fn add_message(&mut self, role: ChatRole, content: impl Into<String>) {
        // Reset user_scrolled on new user message to re-enable auto-scroll
        if role == ChatRole::User {
            self.user_scrolled = false;
        }
        let msg = ChatMessage::new(role, content);
        // Auto-collapse based on role's default_collapsed()
        if msg.role.default_collapsed() {
            self.collapsed_ids.insert(msg.id.clone());
        }
        self.messages.push(msg);
        self.scroll_to_bottom();
        self.update_session_info();
        self.dirty = true;
    }

    /// Add a pre-collapsed message (e.g., tool call card).
    pub fn add_collapsed_message(&mut self, role: ChatRole, content: impl Into<String>) {
        let msg = ChatMessage::new_collapsed(role, content);
        self.collapsed_ids.insert(msg.id.clone());
        self.messages.push(msg);
        self.scroll_to_bottom();
        self.update_session_info();
        self.dirty = true;
    }

    /// Append text to the last assistant message (for streaming/typewriter).
    ///
    /// During streaming, text is buffered in `stream_buffer` and only applied
    /// when `flush_stream_buffer()` is called (at throttled intervals).
    /// This prevents 100+ redraws/second during fast streaming.
    pub fn append_to_last_assistant(&mut self, text: &str) {
        // Buffer the chunk instead of immediately appending
        self.stream_buffer.push_str(text);
        // Don't mark dirty yet — flush_stream_buffer() will do that
    }

    /// Flush the stream buffer — apply buffered text to the last assistant message.
    ///
    /// Called by the main loop at throttled intervals (~10fps) during streaming,
    /// and on Complete events to ensure final text is displayed.
    pub fn flush_stream_buffer(&mut self) {
        if self.stream_buffer.is_empty() {
            return;
        }
        if let Some(last) = self.messages.last_mut() {
            if last.role == ChatRole::Assistant {
                // Invalidate the cache entry for the streaming message
                self.render_cache.invalidate(&last.id);
                last.content.push_str(&self.stream_buffer);
                self.stream_buffer.clear();
                self.scroll_to_bottom();
                // dirty is set by scroll_to_bottom()
                self.last_stream_flush = std::time::Instant::now();
                return;
            }
        }
        // No assistant message to append to — create one from buffer
        let buffered_text = self.stream_buffer.clone();
        self.stream_buffer.clear();
        self.add_message(ChatRole::Assistant, buffered_text);
        self.last_stream_flush = std::time::Instant::now();
    }

    /// Scroll to the bottom of the chat area.
    /// Disables user_scrolled so auto-scroll-to-bottom takes effect on next render.
    fn scroll_to_bottom(&mut self) {
        self.user_scrolled = false;
        self.dirty = true;
    }

    /// Update current session info (message count, preview) from current state.
    pub fn update_session_info(&mut self) {
        let msg_count = self.messages.len();
        let preview = self.messages.iter()
            .find(|m| m.role == ChatRole::User)
            .map(|m| {
                let content = &m.content;
                content.chars().take(20).collect::<String>()
            })
            .unwrap_or_default();

        // Update the active session entry
        for session in &mut self.sessions {
            if session.is_active {
                session.message_count = msg_count;
                session.preview = preview;
                break;
            }
        }
    }

    /// Add a new session to the sessions list (e.g., after /clear or /new).
    /// Marks the previous active session as inactive and creates a new one.
    #[allow(dead_code)]
    pub fn add_new_session(&mut self, new_session_id: String) {
        // Mark previous sessions as inactive
        for session in &mut self.sessions {
            session.is_active = false;
        }

        let short_id = new_session_id[..8.min(new_session_id.len())].to_string();
        self.sessions.push(SessionInfo {
            short_id,
            full_id: new_session_id.clone(),
            message_count: 0,
            is_active: true,
            preview: String::new(),
        });
        self.session_id = new_session_id;
    }

    /// Toggle collapse state of a message by ID.
    #[allow(dead_code)]
    pub fn toggle_collapse(&mut self, id: &str) {
        if self.collapsed_ids.contains(id) {
            self.collapsed_ids.remove(id);
        } else {
            self.collapsed_ids.insert(id.to_string());
        }
        self.render_cache.invalidate(id);
        self.dirty = true;
    }

    /// Save current input state to undo stack (for Ctrl+Z).
    fn save_undo_state(&mut self) {
        self.input_undo_stack.push(self.input.clone());
        // Keep undo stack bounded (max 50 entries)
        if self.input_undo_stack.len() > 50 {
            self.input_undo_stack.remove(0);
        }
    }

    /// Undo last input change (Ctrl+Z). Returns true if undo was performed.
    fn undo_input(&mut self) -> bool {
        if let Some(prev) = self.input_undo_stack.pop() {
            self.input = prev;
            true
        } else {
            false
        }
    }

    /// Handle a key event. Returns Some(user_input) if a message should be sent.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> Option<String> {
        // Only handle key press events (ignore release/repeat)
        if key.kind != KeyEventKind::Press {
            return None;
        }

        // Note: approval keys are handled in mod.rs's handle_approval_key()
        // before dispatching to handle_key_event()

        // Handle command autocomplete if active
        if self.command_autocomplete {
            let result = self.handle_autocomplete_key(key);
            // Always mark dirty after autocomplete key handling — index changes,
            // input changes, etc. all need visual feedback
            self.dirty = true;
            return result;
        }

        // Dispatch based on current input mode
        let result = match self.input_mode {
            InputMode::SingleLine => self.handle_singleline_key(key),
            InputMode::MultiLineVim { cursor_position, mode } => {
                self.handle_vim_key(key, cursor_position, mode)
            }
        };

        // Any key press that wasn't filtered out likely changed some state
        self.dirty = true;
        result
    }

    /// Handle key presses when command autocomplete is active.
    fn handle_autocomplete_key(&mut self, key: KeyEvent) -> Option<String> {
        let suggestions = self.get_command_suggestions();
        let suggestion_count = suggestions.len();

        // Clamp the index if it's out of bounds (e.g., list shrunk after typing)
        if suggestion_count > 0 && self.command_autocomplete_index >= suggestion_count {
            self.command_autocomplete_index = suggestion_count - 1;
        }

        match (key.modifiers, key.code) {
            // ↑: navigate up in suggestions (wraps to bottom)
            (KeyModifiers::NONE, KeyCode::Up) => {
                if suggestion_count > 0 {
                    if self.command_autocomplete_index == 0 {
                        self.command_autocomplete_index = suggestion_count - 1;
                    } else {
                        self.command_autocomplete_index -= 1;
                    }
                }
                None
            }
            // ↓: navigate down in suggestions (wraps to top)
            (KeyModifiers::NONE, KeyCode::Down) => {
                if suggestion_count > 0 {
                    if self.command_autocomplete_index >= suggestion_count - 1 {
                        self.command_autocomplete_index = 0;
                    } else {
                        self.command_autocomplete_index += 1;
                    }
                }
                None
            }
            // Enter or Tab: accept selected suggestion
            (KeyModifiers::NONE, KeyCode::Enter) | (KeyModifiers::NONE, KeyCode::Tab) => {
                self.accept_autocomplete();
                None
            }
            // Esc: close autocomplete
            (_, KeyCode::Esc) => {
                self.command_autocomplete = false;
                self.command_autocomplete_index = 0;
                None
            }
            // Backspace: delete char before cursor, close autocomplete if input no longer starts with /
            (KeyModifiers::NONE, KeyCode::Backspace) | (KeyModifiers::SHIFT, KeyCode::Backspace) => {
                if self.input_cursor_pos > 0 {
                    let prev = prev_char_boundary(&self.input, self.input_cursor_pos);
                    self.input.replace_range(prev..self.input_cursor_pos, "");
                    self.input_cursor_pos = prev;
                }
                if !self.input.starts_with('/') || self.input.is_empty() {
                    self.command_autocomplete = false;
                    self.command_autocomplete_index = 0;
                } else {
                    // Re-clamp index after suggestions may have changed
                    let new_suggestions = self.get_command_suggestions();
                    if new_suggestions.is_empty() {
                        self.command_autocomplete = false;
                        self.command_autocomplete_index = 0;
                    } else if self.command_autocomplete_index >= new_suggestions.len() {
                        self.command_autocomplete_index = new_suggestions.len() - 1;
                    }
                }
                None
            }
            // Char: insert at cursor position and filter (accept both NONE and SHIFT modifiers)
            (KeyModifiers::NONE, KeyCode::Char(c)) | (KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                self.input.insert(self.input_cursor_pos, c);
                self.input_cursor_pos += c.len_utf8();
                // Keep autocomplete active if still matching
                let new_suggestions = self.get_command_suggestions();
                if new_suggestions.is_empty() {
                    self.command_autocomplete = false;
                    self.command_autocomplete_index = 0;
                } else {
                    // Re-clamp index after suggestions may have changed
                    if self.command_autocomplete_index >= new_suggestions.len() {
                        self.command_autocomplete_index = new_suggestions.len() - 1;
                    }
                }
                None
            }
            // Any other key: close autocomplete and process normally
            _ => {
                self.command_autocomplete = false;
                self.command_autocomplete_index = 0;
                self.handle_singleline_key(key)
            }
        }
    }

    /// Handle keys in single-line input mode.
    fn handle_singleline_key(&mut self, key: KeyEvent) -> Option<String> {
        match (key.modifiers, key.code) {
            // Ctrl+Enter: insert newline
            (KeyModifiers::CONTROL, KeyCode::Enter) => {
                self.input.insert(self.input_cursor_pos, '\n');
                self.input_cursor_pos += 1; // '\n' is 1 byte
                None
            }

            // Enter (no modifier): send message
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.input.is_empty() {
                    return None;
                }
                let msg = self.input.clone();
                self.message_history.push(msg.clone());
                self.input.clear();
                self.input_cursor_pos = 0;
                self.message_history.reset();
                Some(msg)
            }

            // Escape: enter vim multi-line mode (Normal mode)
            (_, KeyCode::Esc) => {
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: self.input_cursor_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // Tab: toggle sidebar
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.show_sidebar = !self.show_sidebar;
                None
            }

            // Backspace: delete char before cursor (save undo) — UTF-8 safe
            (KeyModifiers::NONE, KeyCode::Backspace) | (KeyModifiers::SHIFT, KeyCode::Backspace) => {
                self.save_undo_state();
                if self.input_cursor_pos > 0 {
                    let prev = prev_char_boundary(&self.input, self.input_cursor_pos);
                    let char_len = self.input_cursor_pos - prev;
                    self.input.replace_range(prev..self.input_cursor_pos, "");
                    self.input_cursor_pos = prev;
                }
                None
            }

            // Ctrl+C: force quit
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.should_quit = true;
                None
            }

            // Ctrl+L: clear screen
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.messages.clear();
                self.render_cache.invalidate_all();
                None
            }

            // ↑: navigate message history (only when input is empty)
            (KeyModifiers::NONE, KeyCode::Up) => {
                if self.input.is_empty() {
                    if let Some(msg) = self.message_history.navigate_up() {
                        self.input = msg.to_string();
                        self.input_cursor_pos = self.input.len();
                    }
                }
                None
            }

            // ↓: navigate message history (only when input is empty)
            (KeyModifiers::NONE, KeyCode::Down) => {
                if self.input.is_empty() {
                    if let Some(msg) = self.message_history.navigate_down() {
                        self.input = msg;
                        self.input_cursor_pos = self.input.len();
                    }
                }
                None
            }

            // ←: move cursor left (one full Unicode character)
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.input_cursor_pos = prev_char_boundary(&self.input, self.input_cursor_pos);
                None
            }

            // →: move cursor right (one full Unicode character)
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.input_cursor_pos = next_char_boundary(&self.input, self.input_cursor_pos);
                None
            }

            // Ctrl+↑/Ctrl+↓: scroll chat area
            (KeyModifiers::CONTROL, KeyCode::Up) => {
                self.chat_scroll_y = self.chat_scroll_y.saturating_add(3);
                self.user_scrolled = true;
                None
            }
            (KeyModifiers::CONTROL, KeyCode::Down) => {
                self.chat_scroll_y = self.chat_scroll_y.saturating_sub(3);
                self.user_scrolled = true;
                None
            }

            // Ctrl+Z: undo input
            (KeyModifiers::CONTROL, KeyCode::Char('z')) => {
                self.undo_input();
                self.input_cursor_pos = self.input.len();
                None
            }

            // PageUp/PageDown: scroll chat area by viewport height
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.chat_scroll_y = self.chat_scroll_y.saturating_sub(20); // approximate page
                self.user_scrolled = true;
                None
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.chat_scroll_y = self.chat_scroll_y.saturating_add(20);
                self.user_scrolled = true;
                None
            }

            // Ctrl+F: enter search mode
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.search_mode = true;
                self.search_query.clear();
                None
            }

            // Char input (accept both NONE and SHIFT modifiers for uppercase letters)
            // Also trigger autocomplete on /
            (KeyModifiers::NONE, KeyCode::Char(c)) | (KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                self.save_undo_state();
                self.input.insert(self.input_cursor_pos, c);
                self.input_cursor_pos += c.len_utf8();
                // Trigger command autocomplete when user types /
                if c == '/' || (self.input.starts_with('/') && self.get_command_suggestions().len() > 0) {
                    self.command_autocomplete = true;
                    self.command_autocomplete_index = 0;
                }
                None
            }

            _ => None,
        }
    }

    /// Handle keys in multi-line vim mode.
    fn handle_vim_key(&mut self, key: KeyEvent, cursor_position: usize, mode: VimMode) -> Option<String> {
        match mode {
            VimMode::Normal => self.handle_vim_normal_key(key, cursor_position),
            VimMode::Insert => self.handle_vim_insert_key(key, cursor_position),
        }
    }

    /// Handle keys in vim Normal mode.
    fn handle_vim_normal_key(&mut self, key: KeyEvent, cursor_position: usize) -> Option<String> {
        // Check for pending dd command (first 'd' was pressed)
        if self.vim_pending_cmd == Some('d') {
            self.vim_pending_cmd = None;
            match (key.modifiers, key.code) {
                // dd: delete current line
                (KeyModifiers::NONE, KeyCode::Char('d')) => {
                    self.save_undo_state();
                    let line_start = find_line_start(&self.input, cursor_position);
                    let line_end = find_line_end(&self.input, cursor_position);
                    // Remove the line content from line_start to line_end
                    // Also remove the newline character if it exists
                    let delete_end = if line_end < self.input.len() && self.input.as_bytes()[line_end] == b'\n' {
                        line_end + 1
                    } else if line_start > 0 && self.input.as_bytes()[line_start - 1] == b'\n' {
                        line_start - 1
                    } else {
                        line_end
                    };
                    let del_range = if delete_end < line_start {
                        // Single line at start — delete from line_start to line_end
                        (line_start, line_end)
                    } else {
                        (line_start.min(delete_end), delete_end.max(line_end))
                    };
                    self.input.replace_range(del_range.0..del_range.1, "");
                    // Place cursor at the start of the next line (or end of input)
                    let new_pos = del_range.0.min(self.input.len());
                    self.input_mode = InputMode::MultiLineVim {
                        cursor_position: new_pos,
                        mode: VimMode::Normal,
                    };
                    return None;
                }
                // Any other key cancels the pending 'd' command
                _ => {
                    // Fall through to normal key handling below
                }
            }
        }

        match (key.modifiers, key.code) {
            // i: enter Insert mode
            (KeyModifiers::NONE, KeyCode::Char('i')) => {
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position,
                    mode: VimMode::Insert,
                };
                None
            }

            // Esc: exit multi-line mode, return to single-line
            (_, KeyCode::Esc) => {
                self.vim_pending_cmd = None;
                self.input_mode = InputMode::SingleLine;
                None
            }

            // d: start dd delete-line command (first press)
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                self.vim_pending_cmd = Some('d');
                None
            }

            // h: move cursor left (one full Unicode char)
            (KeyModifiers::NONE, KeyCode::Char('h')) => {
                let new_pos = prev_char_boundary(&self.input, cursor_position);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // l: move cursor right (one full Unicode char)
            (KeyModifiers::NONE, KeyCode::Char('l')) => {
                let new_pos = next_char_boundary(&self.input, cursor_position);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // j: move cursor down (to next line)
            (KeyModifiers::NONE, KeyCode::Char('j')) => {
                let new_pos = find_next_line_start(&self.input, cursor_position);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // k: move cursor up (to previous line start)
            (KeyModifiers::NONE, KeyCode::Char('k')) => {
                let new_pos = find_prev_line_start(&self.input, cursor_position);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // 0: move to start of current line
            (KeyModifiers::NONE, KeyCode::Char('0')) => {
                let new_pos = find_line_start(&self.input, cursor_position);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // $: move to end of current line (use Shift+4 = '$')
            (KeyModifiers::NONE, KeyCode::Char('$')) => {
                let new_pos = find_line_end(&self.input, cursor_position);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // x: delete character at cursor — UTF-8 safe
            (KeyModifiers::NONE, KeyCode::Char('x')) => {
                if cursor_position < self.input.len() && self.input.is_char_boundary(cursor_position) {
                    let next = next_char_boundary(&self.input, cursor_position);
                    self.input.replace_range(cursor_position..next, "");
                }
                // Cursor stays at same position (or adjusts if at end)
                let new_pos = cursor_position.min(self.input.len());
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: new_pos,
                    mode: VimMode::Normal,
                };
                None
            }

            // Enter: send message (in Normal mode, Enter sends)
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.input.is_empty() {
                    return None;
                }
                let msg = self.input.clone();
                self.message_history.push(msg.clone());
                self.input.clear();
                self.input_mode = InputMode::SingleLine;
                self.message_history.reset();
                Some(msg)
            }

            // Ctrl+C: cancel and return to single-line
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.input.clear();
                self.input_mode = InputMode::SingleLine;
                None
            }

            _ => None,
        }
    }

    /// Handle keys in vim Insert mode.
    fn handle_vim_insert_key(&mut self, key: KeyEvent, cursor_position: usize) -> Option<String> {
        match (key.modifiers, key.code) {
            // Esc: return to Normal mode
            (_, KeyCode::Esc) => {
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position,
                    mode: VimMode::Normal,
                };
                None
            }

            // Enter: insert newline (in Insert mode, Enter = newline)
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.input.insert(cursor_position, '\n');
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: cursor_position + 1,
                    mode: VimMode::Insert,
                };
                None
            }

            // Backspace: delete char before cursor — UTF-8 safe
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if cursor_position > 0 {
                    let prev = prev_char_boundary(&self.input, cursor_position);
                    self.input.replace_range(prev..cursor_position, "");
                    self.input_mode = InputMode::MultiLineVim {
                        cursor_position: prev,
                        mode: VimMode::Insert,
                    };
                }
                None
            }

            // Char input: insert at cursor position — advance by len_utf8
            (KeyModifiers::NONE, KeyCode::Char(c)) | (KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                self.input.insert(cursor_position, c);
                self.input_mode = InputMode::MultiLineVim {
                    cursor_position: cursor_position + c.len_utf8(),
                    mode: VimMode::Insert,
                };
                None
            }

            // Ctrl+C: cancel and return to single-line
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.input.clear();
                self.input_mode = InputMode::SingleLine;
                None
            }

            _ => None,
        }
    }

}

// ─── UTF-8 Cursor Helpers ──────────────────────────────────────────────────

/// Find the byte offset of the previous character boundary before `pos`.
/// Used for Left arrow and Backspace — moves cursor one full Unicode char left.
fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Find the byte offset of the next character boundary after `pos`.
/// Used for Right arrow — moves cursor one full Unicode char right.
fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p.min(s.len())
}

// ─── Vim Navigation Helpers ────────────────────────────────────────────────

/// Find the start of the line containing the given position.
fn find_line_start(input: &str, pos: usize) -> usize {
    // Search backwards for the last newline before pos
    if pos == 0 {
        return 0;
    }
    input[..pos]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0)
}

/// Find the end of the line containing the given position.
fn find_line_end(input: &str, pos: usize) -> usize {
    // Search forward for the next newline after pos, or end of string
    let search_start = pos;
    if search_start >= input.len() {
        return input.len();
    }
    input[search_start..]
        .find('\n')
        .map(|idx| search_start + idx)
        .unwrap_or(input.len())
}

/// Find the start of the next line (for j movement).
fn find_next_line_start(input: &str, pos: usize) -> usize {
    // Find current line end, then the next line start
    let line_end = find_line_end(input, pos);
    if line_end < input.len() {
        line_end + 1 // Skip the newline
    } else {
        pos // Already at last line, stay
    }
}

/// Find the start of the previous line (for k movement).
fn find_prev_line_start(input: &str, pos: usize) -> usize {
    // Find current line start, then find the newline before it
    let current_start = find_line_start(input, pos);
    if current_start == 0 {
        return 0; // Already at first line
    }
    // The newline before current_start is at current_start - 1
    // Find start of line before that newline
    find_line_start(input, current_start - 1)
}
