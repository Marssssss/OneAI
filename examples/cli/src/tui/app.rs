//! App state structure and event handling for the OneAI TUI.
//!
//! Contains the main App state, ChatMessage/ChatRole definitions,
//! and key event handling logic.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::ScrollbarState;

use oneai_agent::ParadigmKind;
use oneai_core::ApprovalRequest;

use oneai_skill::SkillRegistry;

use super::history::MessageHistory;
use super::input_mode::InputMode;

// ─── Interaction Mode ──────────────────────────────────────────────────────

/// TUI interaction mode, cycled with Shift+Tab (Claude Code style).
///
/// - Normal: high-risk tools require explicit approval.
/// - AutoAccept: every tool call is approved silently (no per-call message).
/// - Plan: tool execution is blocked entirely — the agent only produces a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InteractionMode {
    #[default]
    Normal,
    AutoAccept,
    Plan,
}

impl InteractionMode {
    /// Cycle to the next mode: Normal → AutoAccept → Plan → Normal.
    pub fn next(self) -> Self {
        match self {
            InteractionMode::Normal => InteractionMode::AutoAccept,
            InteractionMode::AutoAccept => InteractionMode::Plan,
            InteractionMode::Plan => InteractionMode::Normal,
        }
    }
}

// ─── Work Timer ────────────────────────────────────────────────────────────

/// 整轮工作耗时计时器。
///
/// `start` 在 agent 开始思考（`is_thinking` 置 true）时记录；`last` 保留最近一次
/// 完成耗时，用于完成后暗色显示"这次花了多久"。新一轮提交时 `start_run` 清空
/// `last`。所有调用都在主循环线程，故 `Instant` 无需同步原语。
#[derive(Default, Clone)]
pub struct WorkTimer {
    /// `Some` 表示正在工作中。
    pub start: Option<std::time::Instant>,
    /// 最近一次完成耗时（完成后保留，新一轮开始时清空）。
    pub last: Option<std::time::Duration>,
}

impl WorkTimer {
    /// 开始一轮工作：记录起点，清空上一次保留值。
    pub fn start_run(&mut self) {
        self.start = Some(std::time::Instant::now());
        self.last = None;
    }

    /// 结束一轮工作：把 elapsed 存入 `last`，清空 `start`。
    pub fn stop_run(&mut self) {
        if let Some(t) = self.start.take() {
            self.last = Some(t.elapsed());
        }
    }

    /// 显示用时长：工作中返回实时 elapsed，否则返回最近一次 `last`。
    pub fn display(&self) -> Option<std::time::Duration> {
        self.start.map(|t| t.elapsed()).or(self.last)
    }

    /// 是否正在工作（用于选择亮/暗色）。
    pub fn is_running(&self) -> bool {
        self.start.is_some()
    }
}

// ─── Render Scheduler ─────────────────────────────────────────────────────

/// 单一渲染去抖调度器——合并旧 `dirty` 标志 + `stream_buffer` flush 两套并行机制。
///
/// 主循环在 `should_draw()` 真时 `flush_stream_buffer()` → `terminal.draw()` → `clear()`，
/// poll 超时按 `deadline()` 收窄使循环恰在 deadline 唤醒。
///
/// - `request_render()`（即时）：设 requested + 清 deadline → 本轮立刻 draw
///   （按键/交互路径，零延迟）。等价旧 `dirty = true`。
/// - `request_render_debounced()`（流式 token / spinner）：设 requested；若 deadline 未
///   武装则武装 `now + RENDER_FRAME_INTERVAL`，窗口内后续请求被合并（不延后），
///   实现"批量突变→少 redraw"的去抖。
///
/// 闲置（无 request）= 不 draw；流式时 poll 在 deadline 唤醒（≈30fps）。
pub struct RenderScheduler {
    /// 有渲染请求自上次 draw 以来到达。
    requested: bool,
    /// 去抖 deadline：draw 不早于此 Instant；`None` = 即刻可画。
    deadline: Option<std::time::Instant>,
}

/// 流式/spinner 渲染帧间隔（≈30fps）。可调；macOS 原生端同思路用 20fps。
const RENDER_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

impl RenderScheduler {
    /// 首帧必画（对齐旧 `dirty: true` 初值）。
    pub fn new() -> Self {
        Self {
            requested: true,
            deadline: None,
        }
    }

    /// 即时请求渲染（交互路径）：清 deadline，本轮立刻 draw。
    pub fn request_render(&mut self) {
        self.requested = true;
        self.deadline = None;
    }

    /// 去抖请求渲染（流式 token / spinner）：武装 deadline，窗口内合并。
    pub fn request_render_debounced(&mut self) {
        self.requested = true;
        if self.deadline.is_none() {
            self.deadline = Some(std::time::Instant::now() + RENDER_FRAME_INTERVAL);
        }
    }

    /// 是否应在本轮 draw：有请求且 deadline 已到（或无 deadline）。
    pub fn should_draw(&self) -> bool {
        self.requested && self.deadline.is_none_or(|d| std::time::Instant::now() >= d)
    }

    /// draw 后清空请求与 deadline。
    pub fn clear(&mut self) {
        self.requested = false;
        self.deadline = None;
    }

    /// 当前武装的 deadline（供主循环收窄 poll 超时）。
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }
}

impl Default for RenderScheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Slash Commands ───────────────────────────────────────────────────────

/// Supported slash commands for autocomplete.
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show help and available commands"),
    ("/h", "Shortcut for /help"),
    ("/tools", "List registered tools"),
    ("/t", "Shortcut for /tools"),
    ("/skills", "List all available skills"),
    (
        "/skill",
        "Activate, add, remove, or search skills (use /skill <name>)",
    ),
    ("/clear", "Clear conversation and create new session"),
    ("/usage", "Show session token usage and context"),
    ("/context", "Show detailed context window usage breakdown"),
    ("/session", "Show session details"),
    (
        "/paradigm",
        "Switch agent paradigm (ReAct/Plan/Reflect/Explore)",
    ),
    ("/domain", "Switch domain pack (coding/general)"),
    ("/compact", "Compact conversation context"),
    ("/wf", "Workflow commands: list, run, define, show, graph"),
    ("/new", "Create a new session"),
    (
        "/init",
        "Generate project-instruction file (ONEAI.md/AGENTS.md/CLAUDE.md)",
    ),
    ("/tool", "Directly call a tool with JSON args"),
    ("/quit", "Exit the TUI"),
    ("/q", "Shortcut for /quit"),
];

/// Two-level subcommand map for slash autocomplete (issue #30).
///
/// Source of truth mirrors the TUI dispatch tree in `tui/mod.rs` (the
/// `/session`/`/skill`/`/wf`/`/init`/`/domain`/`/paradigm` arms). When the
/// input is `<cmd> ` (command fully typed + trailing space), the autocomplete
/// popup surfaces these subcommands filtered by what the user has typed so
/// far. Commands absent here (`/clear`, `/new`, ...) take free-form args or
/// none, so they offer no二级 list — the popup closes.
pub const SUBCOMMANDS: &[(&str, &[(&str, &str)])] = &[
    (
        "/session",
        &[
            ("resume", "Reload a saved session by id (or short prefix)"),
            ("list", "Show resumable session ids"),
        ],
    ),
    (
        "/skill",
        &[
            ("off", "Deactivate the active skill"),
            ("add", "Register a custom skill (this session)"),
            ("remove", "Remove a skill (this session)"),
            ("info", "Show a skill's details"),
            ("search", "Find relevant skills by query"),
        ],
    ),
    (
        "/wf",
        &[
            ("list", "List defined workflows"),
            ("run", "Run a workflow by name"),
            ("define", "Define a new workflow"),
            ("show", "Show a workflow's steps"),
            ("graph", "Show a workflow's StateGraph"),
            ("status", "Show the running workflow's status"),
            ("history", "Show workflow run history"),
        ],
    ),
    (
        "/init",
        &[
            ("oneai", "Generate ONEAI.md"),
            ("agents", "Generate AGENTS.md"),
            ("claude", "Generate CLAUDE.md"),
        ],
    ),
    (
        "/domain",
        &[
            ("coding", "Coding domain pack"),
            ("general", "General domain pack"),
        ],
    ),
    (
        "/paradigm",
        &[
            ("ReAct", "Reason+act loop"),
            ("Plan", "Plan-then-execute"),
            ("Reflect", "Self-review loop"),
            ("Explore", "Divergent search"),
        ],
    ),
];

impl App {
    /// Get filtered command suggestions based on current input.
    ///
    /// Two-level (issue #30):
    /// - Input `<prefix>` (no space after the command token) → top-level
    ///   commands from `SLASH_COMMANDS` whose name starts with the prefix
    ///   (legacy behavior).
    /// - Input `<cmd> <sub-partial>` (command fully typed + a space, even an
    ///   empty one) → subcommands of `<cmd>` (from `SUBCOMMANDS`) whose name
    ///   starts with `<sub-partial>`. Returned as the full input line
    ///   (`"<cmd> <sub>"`) so the existing `accept_autocomplete` whole-line
    ///   overwrite and `draw_command_popup` renderer keep working unchanged.
    pub fn get_command_suggestions(&self) -> Vec<(String, &'static str)> {
        let input = &self.input;
        if !input.starts_with('/') {
            return Vec::new();
        }
        // Strip the leading '/', split off the first token (command name) from
        // the rest. `splitn(2, ' ')` keeps trailing space semantics: a bare
        // trailing space yields `rest = Some("")` → all subcommands surface.
        let after_slash = &input[1..];
        let mut split = after_slash.splitn(2, ' ');
        let cmd_name = split.next().unwrap_or("");
        let rest = split.next();

        match rest {
            None => {
                // Still completing the command name — prefix includes '/'.
                SLASH_COMMANDS
                    .iter()
                    .filter(|(cmd, _)| cmd.starts_with(input.as_str()))
                    .map(|&(cmd, desc)| (cmd.to_string(), desc))
                    .collect()
            }
            Some(sub_partial) => {
                // Command fully typed + space → enumerate its subcommands.
                let full_cmd = format!("/{}", cmd_name);
                let Some((_, subs)) = SUBCOMMANDS.iter().find(|(c, _)| *c == full_cmd) else {
                    // No fixed subcommand list for this command (e.g. /clear,
                    // /tool <json>, /session resume <free id>) → no popup.
                    return Vec::new();
                };
                subs.iter()
                    .filter(|(sub, _)| sub.starts_with(sub_partial))
                    .map(|(sub, desc)| (format!("{} {}", full_cmd, sub), *desc))
                    .collect()
            }
        }
    }

    /// Accept the currently selected autocomplete suggestion.
    ///
    /// Overwrites the input with the whole-line suggestion (legacy behavior),
    /// then adapts the post-accept state for two-level flow (issue #30):
    /// - A top-level command that has subcommands → append a trailing space
    ///   and keep autocomplete open so the二级 list surfaces immediately.
    /// - A subcommand → append a trailing space (room for the next free-form
    ///   arg like an id/name) and close the popup.
    /// - A plain top-level command (no subcommands) → as-is, close.
    pub fn accept_autocomplete(&mut self) {
        let suggestions = self.get_command_suggestions();
        if suggestions.is_empty() {
            return;
        }
        let idx = self.command_autocomplete_index.min(suggestions.len() - 1);
        let chosen = suggestions[idx].0.clone();
        let is_subcommand = chosen.contains(' ');
        let top_has_subs = !is_subcommand && SUBCOMMANDS.iter().any(|(c, _)| *c == chosen);

        self.input = if is_subcommand || top_has_subs {
            format!("{} ", chosen)
        } else {
            chosen
        };
        self.input_cursor_pos = self.input.len();

        if top_has_subs {
            // Reopen so the二级 subcommand list appears right away.
            self.command_autocomplete = true;
            self.command_autocomplete_index = 0;
        } else {
            self.command_autocomplete = false;
            self.command_autocomplete_index = 0;
        }
    }
}

/// Check if a tool name is a file operation tool that should display content.
#[allow(dead_code)]
pub fn is_file_operation_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "file_read"
            | "read"
            | "edit_file"
            | "file_edit"
            | "edit"
            | "file_write"
            | "write"
            | "notebook_edit"
            | "list_directory"
            | "ls"
    )
} // ─── Token Usage ──────────────────────────────────────────────────────────

/// Token usage tracking for the session.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub total: u32,
    /// Whether these values are estimated (from character count) rather than actual API-reported.
    pub is_estimated: bool,
    /// Input tokens served from the provider's prompt cache (`cache_read_input_tokens`).
    /// 0 for providers/calls without prompt caching.
    pub cache_read: u32,
    /// Input tokens written into the cache (`cache_creation_input_tokens`).
    pub cache_creation: u32,
    /// Per-call cache-hit ratio of the MOST RECENT inference (`cache_read /
    /// prompt_tokens` for that single call). Unlike the session aggregate
    /// (`cache_hit_ratio()`, which sums across all calls and is diluted by the
    /// unavoidable 0%-hit cold-start call), this is the undiluted single-call
    /// value — the metric the user actually wants ("单次推理复用 token 数 /
    /// 单次推理 prompt token 数"). 0 until the first inference reports cache.
    pub last_hit_ratio: f64,
}

impl TokenUsage {
    pub fn new() -> Self {
        Self::default()
    }

    /// **Session-aggregate** cache-hit ratio in [0, 1]: `Σcache_read /
    /// Σprompt_tokens` across every inference this session. `prompt_tokens` is
    /// the total input footprint per call (providers normalize it to include
    /// cached tokens — OpenAI reports the total directly, Anthropic sums
    /// `input + cache_read + cache_creation`). Matches the OpenAI dashboard's
    /// `cached_tokens / prompt_tokens` and
    /// [`oneai_core::usage::UsageSummary::cache_hit_ratio`]. Returns 0 when no
    /// cache read is reported or no prompt tokens.
    ///
    /// **Caveat:** as a session aggregate this is diluted by the unavoidable
    /// cold-start call (whose `cache_read` is 0 but whose `prompt_tokens` still
    /// enters the denominator). For the undiluted single-call value use
    /// [`TokenUsage::last_hit_ratio`].
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.cache_read == 0 {
            return 0.0;
        }
        let denom = (self.prompt as u64).max(1);
        (self.cache_read as f64) / (denom as f64)
    }

    /// Format token count for display (e.g., "1.2k" for 1200).
    /// If estimated, prefix with ~ (e.g., "~1.2k").
    #[allow(dead_code)]
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
    /// The oneshot channel to send the interaction response back.
    /// This is optional because it gets consumed when the user responds.
    pub response_tx: Option<tokio::sync::oneshot::Sender<oneai_core::InteractionResponse>>,
}

/// State for a pending plan-decision request (a planning tradeoff the user must
/// resolve). Set when an `InteractionRequest::PlanDecision` arrives.
#[derive(Debug)]
pub struct PlanDecisionState {
    pub question: String,
    pub context: String,
    pub options: Vec<oneai_core::DecisionOption>,
    /// Currently highlighted option index.
    pub selected: usize,
    /// Reply channel for the chosen `InteractionResponse` (Choose/Revise/Abort).
    pub reply_tx: tokio::sync::oneshot::Sender<oneai_core::InteractionResponse>,
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
            ChatRole::ToolInvocation { result, .. } => match result {
                Some((success, _)) => {
                    if *success {
                        TOOL_RESULT_SUCCESS_COLOR
                    } else {
                        TOOL_RESULT_FAILURE_COLOR
                    }
                }
                None => TOOL_CALL_COLOR,
            },
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
            ChatRole::ToolInvocation { result, .. } => match result {
                Some((success, _)) => {
                    if *success {
                        TOOL_RESULT_SUCCESS_COLOR
                    } else {
                        TOOL_RESULT_FAILURE_COLOR
                    }
                }
                None => TOOL_CALL_BORDER,
            },
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
            ChatRole::ToolInvocation {
                tool_name, result, ..
            } => {
                // When result is pending, show tool-specific call icon
                // When result arrived, show success/failure icon
                match result {
                    Some((success, _)) => {
                        if *success {
                            "✅"
                        } else {
                            "❌"
                        }
                    }
                    None => match tool_name.as_str() {
                        "calculator" => "🧮",
                        "grep" | "search" => "🔍",
                        "edit_file" | "file_edit" => "✏️",
                        "read_file" | "file_read" => "📄",
                        "glob" | "file_glob" => "📂",
                        "shell" => "🖥️",
                        "list_directory" => "📂",
                        "web_fetch" => "🌐",
                        _ => "🔧",
                    },
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
    ///
    /// Tool invocations: collapsed while pending (no result yet), and collapsed
    /// when the result exceeds `COLLAPSE_THRESHOLD` lines (so the default view
    /// is a 5-line preview with an expand button). Short results render in full.
    /// Thinking defaults to collapsed, but is auto-expanded on the first real
    /// thinking fragment (see `process_observer_event`).
    pub fn default_collapsed(&self) -> bool {
        use super::theme::COLLAPSE_THRESHOLD;
        match self {
            ChatRole::ToolInvocation { result, .. } => {
                match result {
                    None => true, // Pending tool call — collapsed while executing
                    Some((_, content)) => content.lines().count() > COLLAPSE_THRESHOLD,
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

    /// 渲染去抖调度器（取代旧 `dirty` 标志，统一 draw 门控 + stream flush 时机）。
    pub render: RenderScheduler,

    /// Issue #18 self-heal: set to force the *next* draw to do a full-viewport
    /// repaint (ratatui `invalidate_viewport` — reset previous buffer +
    /// `CellDiffOption::AlwaysUpdate`) instead of an incremental diff. This
    /// heals any buffer↔terminal desync (the cause of residue that doesn't
    /// clear on redraw — e.g. after streaming clobbered a wide cell, or after
    /// raw terminal ops). Set on desync-trigger events (scroll, resize,
    /// stream-end, /clear); consumed in the main draw path.
    pub invalidate_next_draw: bool,

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

    /// Shared skill registry — the SAME `Arc` held by the oneai-app `App`, so
    /// `/skill` mutations (register/remove/activate) are visible to the
    /// AgentLoop and the `skill` tool without copying.
    pub skill_registry: Arc<SkillRegistry>,

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

    /// 整轮工作耗时计时器。与 `is_thinking` 同步：`start_thinking`/`stop_thinking`
    /// helper 同时维护两者，渲染层用 `work_timer.display()` 显示实时或最近耗时。
    pub work_timer: WorkTimer,

    // ─── Enhanced fields ──────────────────────────────────────────────────
    /// Current input mode (single-line).
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

    /// IDs of messages that are currently collapsed.
    pub collapsed_ids: HashSet<String>,

    /// Message history for ↑↓ navigation.
    pub message_history: MessageHistory,

    /// Pending approval request (if any) — the one currently displayed.
    pub approval_pending: Option<ApprovalPendingState>,

    /// Approval requests queued behind the currently-displayed one. Parallel
    /// tool calls can each require approval in the same iteration; without this
    /// queue the later arrivals would overwrite `approval_pending` and drop the
    /// earlier oneshot reply channels (Issue #20 — the user could only answer
    /// the last approval, the rest silently failed with `ChannelDropped`).
    pub approval_queue: VecDeque<ApprovalPendingState>,

    /// Selected option index in approval UI (0=Y, 1=N, 2=M, 3=A).
    pub approval_selected_index: usize,

    /// Session-level approval allowlist (tool names auto-approved).
    pub session_allowlist: HashSet<String>,

    /// Current interaction mode (Normal / Auto-accept / Plan). Cycled with Shift+Tab.
    pub interaction_mode: InteractionMode,

    /// Sidebar verbosity. User mode (default, false) hides the Tools & Paradigm
    /// sections — they expose internal state users don't need (tool calls already
    /// render inline in chat; paradigm shows in the context bar). Verbose mode
    /// (toggled with `v`) restores the full developer observability panel.
    /// Skills/Sessions/Cost/Context always show regardless.
    pub verbose_sidebar: bool,

    /// Whether the one-time mode-prompt tip has been shown on first submission.
    pub mode_prompt_shown: bool,

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

    /// Plain text of each visible chat row, top-to-bottom (built in
    /// `draw_chat`). Indexed by screen row relative to the chat rect top.
    /// Used by in-app Shift+drag selection to copy the selected rows to the
    /// clipboard — the app does its own selection rather than relying on the
    /// terminal's Shift-bypass (which not all terminals honor).
    pub visible_line_text: Vec<String>,

    /// In-app text selection: `(start, end)` visible-row indices (end
    /// inclusive), built while the user holds Shift + drags in the chat area.
    /// `None` when no selection is active.
    pub text_selection: Option<(usize, usize)>,

    /// True while a Shift+drag selection is in progress (between mouse-down
    /// and mouse-up). Distinguishes a drag from a plain click.
    pub text_selecting: bool,

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
    /// Draw path (`run_main_loop` → `should_draw` → `flush_stream_buffer`) applies
    /// it at the render-deadline cadence (~30fps) — the `RenderScheduler` owns the
    /// draw-throttle role; this buffer only batches content appends.
    pub stream_buffer: String,

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

    /// Live plan state — the task list the model mutates via the `task_*`
    /// control tools. Rendered as a persistent panel above the chat area.
    /// None when no plan exists.
    pub plan_state: Option<oneai_agent::PlanState>,
    /// A plan submitted via `exit_plan_mode`, awaiting the user's
    /// accept/reject decision. While set, the TUI shows an accept/reject UI.
    /// The oneshot sender returns the decision to the (blocked) AgentLoop.
    pub pending_plan: Option<(
        String,
        Vec<oneai_agent::PlanStep>,
        Option<tokio::sync::oneshot::Sender<oneai_core::InteractionResponse>>,
    )>,
    /// Pending plan-decision request (a tradeoff the user must resolve during
    /// planning). Set when an `InteractionRequest::PlanDecision` arrives.
    pub plan_decision_pending: Option<PlanDecisionState>,
    /// Selected option in the plan-review UI (0=Accept, 1=Revise, 2=Reject).
    pub plan_approval_selected_index: usize,
    /// Vertical scroll offset of the plan-approval popup body.
    /// Lets the user page through the full plan_text + steps that don't fit
    /// the default compact window.
    pub plan_approval_scroll: usize,
    /// When set, the plan-review popup is collecting Revise feedback text
    /// instead of navigating buttons.
    pub plan_revise_input: Option<String>,
}

impl App {
    pub fn new(
        provider_info: String,
        model_name: String,
        tool_names: Vec<String>,
        session_id: String,
        skill_registry: Arc<SkillRegistry>,
    ) -> Self {
        // Compute context window from model name before it's moved into the struct
        let context_window_size =
            oneai_core::token_counter::infer_context_window_for_tokenizer(model_name.as_str());
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
            render: RenderScheduler::new(), // First frame must always draw
            invalidate_next_draw: false,    // Issue #18: no desync to heal at startup
            show_sidebar: true,
            input: String::new(),
            input_cursor_pos: 0,
            messages: Vec::new(),
            scrollbar_state: ScrollbarState::default(),
            tool_names,
            skill_registry,
            skill_names: Vec::new(),
            active_skill: None,
            provider_info,
            model_name,
            session_id,
            sessions: vec![initial_session],
            current_domain: "coding".to_string(),
            context_window_size,
            is_thinking: false,
            work_timer: WorkTimer::default(),

            input_mode: InputMode::default(),
            active_paradigm: ParadigmKind::ReAct,
            current_iteration: 0,
            token_usage: TokenUsage::new(),
            context_tokens: 0,
            context_tokens_is_estimated: false,
            collapsed_ids: HashSet::new(),
            message_history: MessageHistory::new(),
            approval_pending: None,
            approval_queue: VecDeque::new(),
            approval_selected_index: 0,
            session_allowlist: HashSet::new(),
            interaction_mode: InteractionMode::default(),
            verbose_sidebar: false,
            mode_prompt_shown: false,
            spinner_frame: 0,
            chat_scroll_y: 0,
            user_scrolled: false,
            last_chat_rect: ratatui::layout::Rect::default(),
            content_height: 0,
            visible_line_text: Vec::new(),
            text_selection: None,
            text_selecting: false,
            input_undo_stack: Vec::new(),
            search_mode: false,
            command_autocomplete: false,
            command_autocomplete_index: 0,
            search_query: String::new(),
            search_results: Vec::new(),
            search_result_index: 0,
            stream_buffer: String::new(),
            render_cache: MessageRenderCache::new(),
            last_context_accounting: None,
            plan_state: None,
            pending_plan: None,
            plan_decision_pending: None,
            plan_approval_selected_index: 0,
            plan_approval_scroll: 0,
            plan_revise_input: None,
        }
    }

    /// Estimate token count from conversation content when provider returns 0.
    /// Approximate: ~4 characters = 1 token (for English/mixed text).
    pub fn estimate_tokens_from_messages(&self) -> u32 {
        let total_chars: usize = self.messages.iter().map(|m| m.content.len()).sum();
        (total_chars / 4) as u32
    }

    /// Add a chat message and auto-scroll to bottom.
    ///
    /// For User messages, this resets user_scrolled to re-enable auto-scroll.
    /// For other message types (system, tool, etc.), auto-scroll behavior is preserved.
    /// Mark the agent as started thinking and (re)start the work timer.
    /// Use this instead of `self.is_thinking = true;` so the timer stays in sync.
    pub fn start_thinking(&mut self) {
        self.is_thinking = true;
        self.work_timer.start_run();
    }

    /// Mark the agent as stopped thinking and stop the work timer (retaining
    /// the last run's duration in `work_timer.last` for dim post-completion display).
    pub fn stop_thinking(&mut self) {
        self.is_thinking = false;
        self.work_timer.stop_run();
    }

    /// 即时请求渲染（交互路径，零延迟）——等价旧 `dirty = true`。
    pub fn request_render(&mut self) {
        self.render.request_render();
    }

    /// Issue #18: 请求下一帧做全量重绘（自愈 desync）。在滚动/resize/流式结束/
    /// `/clear` 等可能让 ratatui buffer 与终端失步的点调用。主循环 draw 前消费。
    pub fn request_invalidate(&mut self) {
        self.invalidate_next_draw = true;
        self.request_render();
    }

    /// 去抖请求渲染（流式 token / spinner）——武装 deadline，窗口内合并。
    pub fn request_render_debounced(&mut self) {
        self.render.request_render_debounced();
    }

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
        self.request_render();
    }

    /// Add a pre-collapsed message (e.g., tool call card).
    pub fn add_collapsed_message(&mut self, role: ChatRole, content: impl Into<String>) {
        let msg = ChatMessage::new_collapsed(role, content);
        self.collapsed_ids.insert(msg.id.clone());
        self.messages.push(msg);
        self.scroll_to_bottom();
        self.update_session_info();
        self.request_render();
    }

    /// Append text to the last assistant message (for streaming/typewriter).
    ///
    /// Text is buffered in `stream_buffer` and applied when the main loop's
    /// `RenderScheduler` reaches its draw deadline (~30fps). The scheduler owns
    /// the draw-throttle role; this only batches content appends.
    pub fn append_to_last_assistant(&mut self, text: &str) {
        self.stream_buffer.push_str(text);
        self.request_render_debounced();
    }

    /// Flush the stream buffer — apply buffered text to the last assistant message.
    ///
    /// Called by the main loop draw path (when `RenderScheduler::should_draw()`)
    /// and on Complete events to ensure final text is displayed. Does not touch
    /// render scheduling — the caller/draw path owns "needs redraw".
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
                // sticky-scroll: only auto-scroll to bottom when the user
                // hasn't manually scrolled up to read history. Resetting
                // `user_scrolled` here would yank the viewport back to the
                // bottom every ~33ms during streaming, making it impossible
                // to browse history mid-stream. The new content still lands
                // in the last assistant bubble; the chat draw keeps the user's
                // scroll position (`chat_scroll_y.min(max_scroll)`) — the
                // growing `max_scroll` simply lets them scroll down to it.
                // Auto-follow is re-enabled by an explicit "return to bottom"
                // trigger: new user message, `End`, or dragging/scrolling to
                // the very bottom (see `handle_singleline_key` / mouse handler).
                if !self.user_scrolled {
                    self.scroll_to_bottom();
                }
                return;
            }
        }
        // No assistant message to append to — create one from buffer
        let buffered_text = self.stream_buffer.clone();
        self.stream_buffer.clear();
        self.add_message(ChatRole::Assistant, buffered_text);
    }

    /// Scroll to the bottom of the chat area.
    /// Disables user_scrolled so auto-scroll-to-bottom takes effect on next render.
    fn scroll_to_bottom(&mut self) {
        self.user_scrolled = false;
    }

    /// Begin / extend the in-app text selection at `row` (a visible chat row
    /// index relative to the chat rect top). Driven by Shift+drag — the app
    /// owns the selection so it works on every terminal, regardless of whether
    /// the terminal honors Shift-bypass for mouse reporting.
    pub fn update_text_selection(&mut self, row: usize) {
        let clamped = row.min(self.visible_line_text.len().saturating_sub(1));
        if self.text_selecting {
            if let Some(sel) = self.text_selection.as_mut() {
                sel.1 = clamped;
            }
            self.request_render();
        } else {
            self.text_selecting = true;
            self.text_selection = Some((clamped, clamped));
            self.request_render();
        }
    }

    /// Finalize the selection: copy the selected rows' plain text to the
    /// system clipboard (via arboard), then clear the selection. Returns the
    /// copied text (empty if nothing selected / clipboard unavailable).
    pub fn copy_selection_to_clipboard(&mut self) -> String {
        let (start, end) = match self.text_selection {
            Some(s) => s,
            None => {
                self.text_selecting = false;
                return String::new();
            }
        };
        let lo = start.min(end);
        let mut hi = start.max(end);
        // Clamp to the last drawn visible rows (the map reflects the prior frame).
        if self.visible_line_text.is_empty() {
            self.text_selection = None;
            self.text_selecting = false;
            return String::new();
        }
        hi = hi.min(self.visible_line_text.len() - 1);
        if lo > hi {
            self.text_selection = None;
            self.text_selecting = false;
            return String::new();
        }
        let text = self.visible_line_text[lo..=hi].join("\n");
        // Create the clipboard handle per-call — cheap, and avoids holding a
        // long-lived platform clipboard handle across the event loop.
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text.clone());
        }
        self.text_selection = None;
        self.text_selecting = false;
        text
    }

    /// Cancel any in-app text selection (e.g. on Esc or new render cycle).
    pub fn clear_text_selection(&mut self) {
        if self.text_selection.is_some() {
            self.text_selection = None;
            self.text_selecting = false;
            self.request_render();
        }
    }

    /// Update current session info (message count, preview) from current state.
    pub fn update_session_info(&mut self) {
        let msg_count = self.messages.len();
        let preview = self
            .messages
            .iter()
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

    /// Add a new session to the sessions list (e.g., after /clear or /new), or
    /// activate an existing one (e.g., `/session resume <id>` of a session that
    /// already appears in the list). Marks every other session inactive. Does
    /// NOT push a duplicate when the id is already present — that was the root
    /// cause of "switching via the TAB bar added an extra row each click" (the
    /// startup DB load now pre-populates `sessions`, so the resumed session is
    /// usually already listed).
    #[allow(dead_code)]
    pub fn add_new_session(&mut self, new_session_id: String) {
        // Mark previous sessions as inactive
        for session in &mut self.sessions {
            session.is_active = false;
        }

        // If this session already has an entry (e.g. loaded from the DB at
        // startup and now resumed), just reactivate it — no duplicate row.
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|s| s.full_id == new_session_id)
        {
            existing.is_active = true;
            existing.message_count = self.messages.len();
            self.session_id = new_session_id;
            return;
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
        self.request_render();
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
    /// Insert pasted text at the cursor without submitting.
    ///
    /// Bracketed-paste content arrives as a single `Event::Paste(String)` once
    /// `EnableBracketedPaste` is active. Previously, multi-line pastes were split
    /// into a keystream where every newline triggered `Enter` → send, so only
    /// the first line went out for inference. This inserts the entire pasted
    /// string at the cursor and never submits — the user reviews then presses
    /// Enter.
    ///
    /// Line endings are normalized to `\n` first. crossterm delivers paste bytes
    /// verbatim (no CR→LF translation), and in raw mode many terminals send `\r`
    /// (or `\r\n`) for line breaks. The rest of the input pipeline — `wrap_input`,
    /// `input_visual_line_count`, Ctrl+Enter — only recognizes `\n`, so without
    /// normalization a multi-line paste collapses to a single visual line.
    pub fn handle_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = normalize_paste_newlines(text);
        if normalized.is_empty() {
            return;
        }
        self.save_undo_state();
        // Only SingleLine mode exists now; insert at the cursor and advance.
        self.input.insert_str(self.input_cursor_pos, &normalized);
        self.input_cursor_pos += normalized.len();
        self.request_render();
    }

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
            self.request_render();
            return result;
        }

        // Dispatch to the single-line input handler (the only input mode).
        let result = self.handle_singleline_key(key);

        // Any key press that wasn't filtered out likely changed some state
        self.request_render();
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
            (KeyModifiers::NONE, KeyCode::Backspace)
            | (KeyModifiers::SHIFT, KeyCode::Backspace) => {
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
            // Ctrl+Enter: insert newline (only delivered by terminals that
            // distinguish it; macOS Terminal.app and others send plain Enter).
            (KeyModifiers::CONTROL, KeyCode::Enter) => {
                self.input.insert(self.input_cursor_pos, '\n');
                self.input_cursor_pos += 1; // '\n' is 1 byte
                None
            }

            // Enter (no modifier): Claude Code-style line continuation — when
            // the cursor sits at the end of the input and the last character
            // is a backslash, Enter consumes that `\` and inserts a newline
            // instead of sending. Otherwise Enter sends the message. This is
            // the reliable way to build multi-line input since Ctrl+Enter is
            // intercepted by the OS/terminal on macOS and never reaches us.
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.input_cursor_pos == self.input.len()
                    && self.input.ends_with('\\')
                    && !self.input.is_empty()
                {
                    self.save_undo_state();
                    self.input.pop(); // drop the trailing backslash
                    self.input.push('\n');
                    self.input_cursor_pos = self.input.len();
                    return None;
                }
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

            // Escape: no-op when idle. (Interrupting a running agent is handled
            // earlier in mod.rs, before this function is reached.)

            // Tab: toggle sidebar
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.show_sidebar = !self.show_sidebar;
                None
            }

            // Backspace: delete char before cursor (save undo) — UTF-8 safe
            (KeyModifiers::NONE, KeyCode::Backspace)
            | (KeyModifiers::SHIFT, KeyCode::Backspace) => {
                self.save_undo_state();
                if self.input_cursor_pos > 0 {
                    let prev = prev_char_boundary(&self.input, self.input_cursor_pos);
                    let _char_len = self.input_cursor_pos - prev;
                    self.input.replace_range(prev..self.input_cursor_pos, "");
                    self.input_cursor_pos = prev;
                }
                None
            }

            // Ctrl+C: clear the input draft; if already empty, quit (Claude
            // Code convention — first press cancels the draft, second press
            // exits). When the agent is running, Ctrl+C is intercepted earlier
            // in mod.rs to interrupt inference instead of reaching here.
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if !self.input.is_empty() {
                    self.save_undo_state();
                    self.input.clear();
                    self.input_cursor_pos = 0;
                    self.message_history.reset();
                } else {
                    self.should_quit = true;
                }
                None
            }

            // Ctrl+L: clear screen
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.messages.clear();
                self.render_cache.invalidate_all();
                None
            }

            // ↑: move the cursor up one logical line (column preserved). On the
            // first line, fall back to message-history navigation only when the
            // input is empty (so a non-empty draft is never discarded).
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(p) = move_cursor_up(&self.input, self.input_cursor_pos) {
                    self.input_cursor_pos = p;
                } else if self.input.is_empty() {
                    if let Some(msg) = self.message_history.navigate_up() {
                        self.input = msg.to_string();
                        self.input_cursor_pos = self.input.len();
                    }
                }
                None
            }

            // ↓: move the cursor down one logical line. On the last line,
            // fall back to history navigation when input is empty.
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(p) = move_cursor_down(&self.input, self.input_cursor_pos) {
                    self.input_cursor_pos = p;
                } else if self.input.is_empty() {
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

            // Home/End: jump chat to top / bottom. End re-enables auto-follow
            // (clears `user_scrolled`) so new streamed content sticks to bottom
            // again — pairs with the sticky-scroll fix in `flush_stream_buffer`.
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.chat_scroll_y = 0;
                self.user_scrolled = true;
                self.request_render();
                None
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.user_scrolled = false;
                self.scroll_to_bottom();
                self.request_render();
                None
            }

            // Ctrl+F: enter search mode
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.search_mode = true;
                self.search_query.clear();
                None
            }

            // v (on empty input): toggle sidebar verbosity. Guarded on empty
            // input so typing 'v' mid-message still inserts the character.
            (KeyModifiers::NONE, KeyCode::Char('v')) if self.input.is_empty() => {
                self.verbose_sidebar = !self.verbose_sidebar;
                if self.verbose_sidebar {
                    self.add_message(
                        ChatRole::System,
                        "🔬 Verbose sidebar: showing Tools & Paradigm (press v to hide)",
                    );
                } else {
                    self.add_message(ChatRole::System,
                        "Sidebar: user mode — Tools & Paradigm hidden (press v for developer panel)");
                }
                None
            }

            // Char input (accept both NONE and SHIFT modifiers for uppercase letters)
            // Also trigger autocomplete on /
            (KeyModifiers::NONE, KeyCode::Char(c)) | (KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                self.save_undo_state();
                self.input.insert(self.input_cursor_pos, c);
                self.input_cursor_pos += c.len_utf8();
                // Trigger command autocomplete when user types /
                if c == '/'
                    || (self.input.starts_with('/') && !self.get_command_suggestions().is_empty())
                {
                    self.command_autocomplete = true;
                    self.command_autocomplete_index = 0;
                }
                None
            }

            _ => None,
        }
    }
}

// ─── UTF-8 Cursor Helpers ──────────────────────────────────────────────────

/// Normalize line endings in pasted text to `\n`.
///
/// crossterm delivers bracketed-paste bytes verbatim; in raw mode many
/// terminals send `\r` (or `\r\n`) for line breaks instead of `\n`. The input
/// pipeline only splits on `\n`, so we collapse CRLF and lone CR to LF.
fn normalize_paste_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

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

// ─── Line Navigation Helpers ───────────────────────────────────────────────

/// Find the start of the line containing the given position.
fn find_line_start(input: &str, pos: usize) -> usize {
    // Search backwards for the last newline before pos
    if pos == 0 {
        return 0;
    }
    input[..pos].rfind('\n').map(|idx| idx + 1).unwrap_or(0)
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

/// Find the start of the previous line.
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

/// Move the cursor up one logical line, preserving the display column
/// (clamped to the previous line's length). Returns `None` if the cursor is
/// already on the first line. The result is snapped to a char boundary so the
/// cursor never splits a grapheme (column is measured in bytes, like the
/// surrounding line helpers).
fn move_cursor_up(input: &str, pos: usize) -> Option<usize> {
    let line_start = find_line_start(input, pos);
    if line_start == 0 {
        return None;
    }
    let col = pos - line_start;
    let prev_start = find_prev_line_start(input, pos);
    let prev_end = find_line_end(input, prev_start);
    let prev_len = prev_end - prev_start;
    let target = prev_start + col.min(prev_len);
    Some(snap_to_char_boundary(input, target))
}

/// Move the cursor down one logical line, preserving the display column.
/// Returns `None` if the cursor is already on the last line.
fn move_cursor_down(input: &str, pos: usize) -> Option<usize> {
    let line_start = find_line_start(input, pos);
    let line_end = find_line_end(input, pos);
    if line_end >= input.len() {
        return None; // on last line
    }
    let col = pos - line_start;
    let next_start = line_end + 1;
    let next_end = find_line_end(input, next_start);
    let next_len = next_end - next_start;
    let target = next_start + col.min(next_len);
    Some(snap_to_char_boundary(input, target))
}

/// Snap a byte offset down to the nearest char boundary at or before it, so a
/// column computed in bytes never lands inside a multi-byte grapheme.
fn snap_to_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_first_frame_must_draw() {
        // new() primes requested=true so the first loop iteration draws (matches
        // the old `dirty: true` initial value).
        let s = RenderScheduler::new();
        assert!(s.should_draw(), "fresh scheduler must be ready to draw");
    }

    #[test]
    fn normalize_paste_collapses_crlf_and_cr() {
        // CRLF → LF
        assert_eq!(normalize_paste_newlines("a\r\nb\r\nc"), "a\nb\nc");
        // lone CR (raw-mode terminals) → LF
        assert_eq!(normalize_paste_newlines("a\rb\r"), "a\nb\n");
        // already-LF passthrough — no allocation path
        assert_eq!(normalize_paste_newlines("a\nb\n"), "a\nb\n");
        // mixed
        assert_eq!(normalize_paste_newlines("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn handle_paste_preserves_multiline_for_render() {
        // Simulate a raw-mode terminal delivering lone-CR line breaks: the input
        // must end up with `\n` separators so wrap_input renders multiple lines
        // (the bug was that it collapsed to one visual line).
        let mut app = test_app();
        app.handle_paste("line1\rline2\rline3");
        assert_eq!(app.input, "line1\nline2\nline3");
        // visual line count must reflect 3 lines, not 1
        assert_eq!(
            crate::tui::render::input::input_visual_line_count(&app.input, 80),
            3
        );
    }

    #[test]
    fn ctrl_c_clears_input_then_quits_on_second_press() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app();
        app.input = "draft text".to_string();
        app.input_cursor_pos = app.input.len();

        // First Ctrl+C clears the draft — must NOT quit.
        let out = app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(out.is_none());
        assert!(app.input.is_empty(), "first Ctrl+C clears the draft");
        assert_eq!(app.input_cursor_pos, 0);
        assert!(!app.should_quit, "first Ctrl+C must not quit");

        // Second Ctrl+C (input now empty) quits.
        let out = app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(out.is_none());
        assert!(app.should_quit, "second Ctrl+C quits");
    }

    #[test]
    fn arrow_up_down_move_cursor_between_lines() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app();
        // "aa\nbb\ncc" — cursor at the very end (after 'cc').
        app.input = "aa\nbb\ncc".to_string();
        app.input_cursor_pos = app.input.len(); // 8, column 2 on the third line

        // Up → third line column 2 → second line column min(2, 2)=2 → byte 5
        // ("aa\nbb\ncc": line2 starts at 3, "bb" is 2 bytes, col 2 = end of "bb").
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input_cursor_pos, 5, "Up from line 3 → line 2 col 2");

        // Up again → first line column min(2, 2)=2 → byte 2 (end of "aa").
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input_cursor_pos, 2, "Up from line 2 → line 1 col 2");

        // Up on the first line: input is non-empty → no movement, no history.
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.input_cursor_pos, 2,
            "Up on first line with non-empty input is a no-op"
        );

        // Down → back to line 2 col 2 → byte 5.
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input_cursor_pos, 5, "Down from line 1 → line 2 col 2");
        // Down → line 3 col 2 → byte 8 (end).
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input_cursor_pos, 8, "Down from line 2 → line 3 col 2");
    }

    #[test]
    fn backslash_enter_inserts_newline_instead_of_sending() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app();
        app.input = "first line\\".to_string();
        app.input_cursor_pos = app.input.len(); // cursor right after the `\`

        // Enter consumes the trailing `\` and inserts a newline — does NOT send.
        let out = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(out.is_none(), "trailing backslash + Enter must not send");
        assert_eq!(app.input, "first line\n", "backslash replaced by newline");
        assert_eq!(app.input_cursor_pos, app.input.len());

        // Without a trailing backslash, Enter sends normally.
        let mut app = test_app();
        app.input = "hello".to_string();
        app.input_cursor_pos = app.input.len();
        let out = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out.as_deref(), Some("hello"));
        assert!(app.input.is_empty());

        // A backslash NOT at the cursor's end position doesn't trigger
        // continuation — Enter sends the message as-is.
        let mut app = test_app();
        app.input = "ab\\cd".to_string(); // backslash mid-string
        app.input_cursor_pos = app.input.len();
        // last char is 'd', not '\' → send
        let out = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out.as_deref(), Some("ab\\cd"));
    }

    #[test]
    fn move_cursor_vertical_preserves_and_clamps_column() {
        // Equal-length lines "aa\nbb": a(0)a(1)\n(2)b(3)b(4).
        // line1 start0 len2, line2 start3 len2 — col 1 preserved both ways.
        assert_eq!(
            move_cursor_down("aa\nbb", 1),
            Some(4),
            "col 1 preserved going down"
        );
        assert_eq!(
            move_cursor_up("aa\nbb", 4),
            Some(1),
            "col 1 preserved going up"
        );
        // Boundary lines → None.
        assert_eq!(move_cursor_up("aa\nbb", 0), None, "first line → None");
        assert_eq!(move_cursor_down("aa\nbb", 4), None, "last line → None");
        // Clamp down: "aaaa\nb" — line1 len4, line2 len1. col 3 → clamp to 1
        // (after 'b', the last line's end). a(0..3)\n(4)b(5).
        assert_eq!(
            move_cursor_down("aaaa\nb", 3),
            Some(6),
            "col 3 clamps to line2 len1"
        );
        // Clamp up: "b\naaaa" — line1 len1, line2 len4. col 3 → clamp to 1
        // (end-of-line1 = the '\n'). b(0)\n(1)a(2..5).
        assert_eq!(
            move_cursor_up("b\naaaa", 5),
            Some(1),
            "col 3 clamps to line1 len1"
        );
    }

    #[test]
    fn arrow_up_falls_back_to_history_when_input_empty() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app();
        app.message_history.push("older msg".to_string());
        assert!(app.input.is_empty());

        // Up on empty input pulls history into the input box.
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "older msg");
        assert_eq!(app.input_cursor_pos, app.input.len());
    }

    #[test]
    fn arrow_up_on_nonempty_single_line_does_not_discard_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app();
        app.message_history.push("older msg".to_string());
        app.input = "current draft".to_string();
        app.input_cursor_pos = app.input.len();

        // Up must not pull history (would overwrite the draft) and must not
        // move the cursor (single line → no line above). No-op.
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.input, "current draft",
            "non-empty draft must survive Up"
        );
        assert_eq!(app.input_cursor_pos, app.input.len());
    }

    #[test]
    fn scheduler_immediate_request_draws_now() {
        // request_render() clears any deadline → should_draw() true immediately,
        // even right after a debounced deadline was armed.
        let mut s = RenderScheduler::new();
        s.clear();
        s.request_render_debounced();
        assert!(s.deadline().is_some(), "debounced arms a deadline");
        s.request_render(); // interactive path overrides debounce → draw now
        assert!(s.should_draw());
        assert!(s.deadline().is_none(), "immediate request clears deadline");
    }

    #[test]
    fn scheduler_debounced_coalesces_into_one_deadline() {
        // Repeated debounced requests within the window must NOT push the deadline
        // later — they coalesce into the first armed deadline.
        let mut s = RenderScheduler::new();
        s.clear();
        s.request_render_debounced();
        let first = s.deadline();
        s.request_render_debounced();
        s.request_render_debounced();
        assert_eq!(
            s.deadline(),
            first,
            "subsequent debounced calls must not extend deadline"
        );
        assert!(first.is_some());
    }

    #[test]
    fn scheduler_clear_resets() {
        let mut s = RenderScheduler::new();
        s.request_render_debounced();
        s.clear();
        assert!(!s.should_draw(), "after clear, nothing to draw");
        assert!(s.deadline().is_none());
    }

    /// Build a minimal App for scroll/streaming tests (no provider, no tools).
    fn test_app() -> App {
        App::new(
            "test".to_string(),
            "test-model".to_string(),
            Vec::new(),
            "session1234567".to_string(),
            std::sync::Arc::new(oneai_skill::SkillRegistry::new()),
        )
    }

    #[test]
    fn streaming_keeps_user_scroll_position() {
        // sticky-scroll: while the user has scrolled up to read history
        // (`user_scrolled = true`), a stream flush must NOT yank the viewport
        // back to the bottom. The new content still lands in the last
        // assistant bubble, but the user's scroll position is preserved.
        let mut app = test_app();
        app.add_message(ChatRole::Assistant, "hello".to_string());
        app.user_scrolled = true;
        app.chat_scroll_y = 5;

        app.append_to_last_assistant(" world");
        app.flush_stream_buffer();

        assert!(
            app.user_scrolled,
            "user_scrolled must remain true mid-stream"
        );
        assert_eq!(app.chat_scroll_y, 5, "scroll position must not be reset");
        assert_eq!(
            app.messages.last().unwrap().content,
            "hello world",
            "streamed text must still be appended to the bubble"
        );
    }

    #[test]
    fn streaming_auto_follows_when_at_bottom() {
        // When the user has NOT scrolled up, streaming auto-follows: the
        // viewport stays pinned to the bottom as new content arrives.
        let mut app = test_app();
        app.add_message(ChatRole::Assistant, "hello".to_string());
        // user_scrolled stays false (default)
        app.append_to_last_assistant(" world");
        app.flush_stream_buffer();

        assert!(!app.user_scrolled, "auto-follow must remain enabled");
        assert_eq!(app.messages.last().unwrap().content, "hello world");
    }

    #[test]
    fn end_key_re_enables_auto_follow() {
        // After scrolling up, pressing End jumps to bottom and re-enables
        // auto-follow for subsequent streamed content.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app();
        app.add_message(ChatRole::Assistant, "hello".to_string());
        app.user_scrolled = true;
        app.chat_scroll_y = 5;

        // `handle_key_event` returns Some(input) only on send; End returns None
        // but clears `user_scrolled` as a side effect.
        let _ = app.handle_key_event(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(!app.user_scrolled, "End must re-enable auto-follow");
    }

    // ─── #9 streaming-flush correctness ───────────────────────────────────
    //
    // Audit conclusion: the AgentLoop fires BOTH `on_stream_chunk` (typewriter,
    // agent_loop.rs:4800) AND `on_direct_answer` (full text, L2155) for one
    // answer. `process_observer_event`'s `DirectAnswer` arm flushes the stream
    // buffer FIRST (mod.rs ~L1856), then dedups via "is there already an
    // Assistant bubble after the last User?". Because the concatenated chunks
    // equal the full DirectAnswer text, the streamed bubble already holds the
    // complete answer and the duplicate DirectAnswer is dropped — no loss, no
    // double bubble. `Complete` flushes again then `retain`s empty thinking
    // bubbles; the assistant survives. These tests lock those invariants.

    #[test]
    fn streaming_chunks_concatenate_no_loss() {
        // Rapid chunks accumulate in stream_buffer; one flush applies them all
        // to a single assistant bubble — no chunk is dropped between the 33ms
        // debounce windows.
        let mut app = test_app();
        app.add_message(ChatRole::User, "q".to_string());
        app.start_thinking();
        app.append_to_last_assistant("Hel");
        app.append_to_last_assistant("lo ");
        app.append_to_last_assistant("world");
        // Not yet flushed — the buffer holds the concatenation.
        assert_eq!(app.stream_buffer, "Hello world");
        app.flush_stream_buffer();
        let last = app.messages.last().expect("an assistant bubble exists");
        assert_eq!(last.role, ChatRole::Assistant);
        assert_eq!(last.content, "Hello world");
    }

    #[test]
    fn direct_answer_flush_does_not_duplicate_bubble() {
        // Simulates the DirectAnswer dedup: after streaming flushed a bubble,
        // a second flush (DirectAnswer's flush-first) is a no-op (buffer empty)
        // and never creates a second assistant bubble.
        let mut app = test_app();
        app.add_message(ChatRole::User, "q".to_string());
        app.start_thinking();
        app.append_to_last_assistant("Hello world");
        app.flush_stream_buffer();
        // DirectAnswer's flush-first runs again:
        app.flush_stream_buffer();
        let n_assistant = app
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Assistant)
            .count();
        assert_eq!(
            n_assistant, 1,
            "must not create a duplicate assistant bubble"
        );
    }

    #[test]
    fn complete_retain_keeps_streamed_assistant() {
        // Complete flushes then retains empty thinking bubbles — the streamed
        // assistant answer must survive the retain.
        let mut app = test_app();
        app.add_message(ChatRole::User, "q".to_string());
        app.start_thinking();
        app.append_to_last_assistant("Hello world");
        app.flush_stream_buffer();
        // Mirror Complete's retain predicate.
        app.messages.retain(|m| {
            !(m.role == ChatRole::Thinking
                && (m.content == "Processing your request..." || m.content.trim().is_empty()))
        });
        assert_eq!(
            app.messages
                .iter()
                .filter(|m| m.role == ChatRole::Assistant)
                .count(),
            1
        );
        assert_eq!(app.messages.last().unwrap().content, "Hello world");
    }

    // ─── #10 in-app Shift+drag selection ────────────────────────────────
    //
    // The app owns the selection + clipboard write so it works on every
    // terminal (no reliance on Shift-bypass). These test the selection range
    // math + joined-text copy; `arboard::Clipboard::new()` is best-effort
    // (no-op where no display server is available, e.g. CI) but the joined
    // text is returned regardless.

    #[test]
    fn text_selection_copies_joined_rows() {
        let mut app = test_app();
        app.visible_line_text = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "delta".to_string(),
        ];
        // Shift+down at row 1, then drag to row 2.
        app.update_text_selection(1);
        assert!(app.text_selecting, "down starts a selection");
        assert_eq!(app.text_selection, Some((1, 1)));
        app.update_text_selection(2); // extend
        assert_eq!(app.text_selection, Some((1, 2)));
        let copied = app.copy_selection_to_clipboard();
        assert_eq!(copied, "beta\ngamma");
        assert!(app.text_selection.is_none(), "selection cleared after copy");
        assert!(!app.text_selecting);
    }

    #[test]
    fn text_selection_clamps_to_visible_rows() {
        let mut app = test_app();
        app.visible_line_text = vec!["only".to_string()];
        // A row way past the viewport clamps to the last visible row.
        app.update_text_selection(50);
        assert_eq!(app.text_selection, Some((0, 0)));
        let copied = app.copy_selection_to_clipboard();
        assert_eq!(copied, "only");
    }

    #[test]
    fn text_selection_reverses_to_upward_drag() {
        // Dragging upward (start lower than end) still yields an in-order range.
        let mut app = test_app();
        app.visible_line_text = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        app.update_text_selection(2);
        app.update_text_selection(0);
        let copied = app.copy_selection_to_clipboard();
        assert_eq!(copied, "a\nb\nc");
    }

    #[test]
    fn token_usage_cache_hit_ratio() {
        // No cache read → 0 (not a misleading NaN/inf).
        let none = TokenUsage {
            prompt: 1000,
            completion: 50,
            total: 1050,
            is_estimated: false,
            cache_read: 0,
            cache_creation: 0,
            ..Default::default()
        };
        assert_eq!(none.cache_hit_ratio(), 0.0);

        // OpenAI semantics: prompt_tokens is the TOTAL input (already includes
        // cached). cache_read=800, prompt=1000 → 800/1000 = 0.8 (matches
        // cached_tokens / prompt_tokens on the OpenAI dashboard).
        let cached = TokenUsage {
            prompt: 1000,
            completion: 50,
            total: 1050,
            is_estimated: false,
            cache_read: 800,
            cache_creation: 0,
            ..Default::default()
        };
        assert!((cached.cache_hit_ratio() - 0.8).abs() < 1e-9);

        // All read, nothing else → 1.0.
        let full = TokenUsage {
            prompt: 500,
            completion: 0,
            total: 500,
            is_estimated: false,
            cache_read: 500,
            cache_creation: 0,
            ..Default::default()
        };
        assert!((full.cache_hit_ratio() - 1.0).abs() < 1e-9);
    }

    // ── #30 two-level slash autocomplete ─────────────────────────────────────

    fn suggestions_for(app: &App) -> Vec<String> {
        app.get_command_suggestions()
            .into_iter()
            .map(|(cmd, _)| cmd)
            .collect()
    }

    #[test]
    fn slash_toplevel_completion_unchanged() {
        // No space after the command token → still completes top-level names.
        let mut app = test_app();
        app.input = "/se".to_string();
        assert!(suggestions_for(&app).iter().any(|s| s == "/session"));
    }

    #[test]
    fn slash_subcommand_list_after_trailing_space() {
        // `<cmd> ` (trailing space) → surface ALL subcommands of that command.
        let mut app = test_app();
        app.input = "/session ".to_string();
        let s = suggestions_for(&app);
        assert!(s.contains(&"/session resume".to_string()));
        assert!(s.contains(&"/session list".to_string()));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn slash_subcommand_filtered_by_partial() {
        let mut app = test_app();
        app.input = "/wf r".to_string();
        let s = suggestions_for(&app);
        assert_eq!(s, vec!["/wf run".to_string()]);
    }

    #[test]
    fn slash_no_subcommands_command_offers_none_after_space() {
        // /clear has no fixed subcommand list → popup closes (free-form args).
        let mut app = test_app();
        app.input = "/clear ".to_string();
        assert!(app.get_command_suggestions().is_empty());
    }

    #[test]
    fn slash_free_form_arg_after_subcommand_offers_none() {
        // `/session resume <free id>` → the third token is free-form; the
        // partial "resume x" matches no subcommand → no popup.
        let mut app = test_app();
        app.input = "/session resume x".to_string();
        assert!(app.get_command_suggestions().is_empty());
    }

    #[test]
    fn accept_toplevel_with_subs_reopens_for_subcommands() {
        // Accepting `/session` (has subs) appends a space and keeps the popup
        // open so the二级 list surfaces immediately.
        let mut app = test_app();
        app.input = "/se".to_string();
        app.command_autocomplete = true;
        app.command_autocomplete_index = 0;
        app.accept_autocomplete();
        assert_eq!(app.input, "/session ");
        assert!(
            app.command_autocomplete,
            "popup must reopen for subcommands"
        );
        // The reopened list must be the subcommand list, not the top-level one.
        assert!(suggestions_for(&app).contains(&"/session resume".to_string()));
    }

    #[test]
    fn accept_subcommand_appends_space_and_closes() {
        // Accepting a subcommand leaves room for the next free-form arg and
        // closes the popup (no fixed list for the arg).
        let mut app = test_app();
        app.input = "/wf r".to_string();
        app.command_autocomplete = true;
        app.command_autocomplete_index = 0;
        app.accept_autocomplete();
        assert_eq!(app.input, "/wf run ");
        assert!(!app.command_autocomplete);
    }

    // ── #30 session TAB bar (display-only) ──────────────────────────────────

    fn app_with_sessions(n: usize) -> App {
        // Build an App with `n` sessions; the last one is active.
        let mut app = test_app();
        app.sessions.clear();
        for i in 0..n {
            app.sessions.push(crate::tui::app::SessionInfo {
                short_id: format!("s{}", i),
                full_id: format!("full-{}", i),
                message_count: i,
                is_active: i + 1 == n, // last is active
                preview: String::new(),
            });
        }
        app
    }

    #[test]
    fn add_new_session_dedupes_existing_id() {
        // Resuming a session already in the list (e.g. one loaded from the DB
        // at startup) must NOT push a duplicate row — it just reactivates the
        // existing entry. Guards against the "switching adds an extra row each
        // time" regression (issue #30).
        let mut app = app_with_sessions(3); // full-0, full-1, full-2(active)
        assert_eq!(app.sessions.len(), 3);
        app.add_new_session("full-0".to_string());
        assert_eq!(app.sessions.len(), 3, "must not add a duplicate row");
        assert_eq!(app.session_id, "full-0");
        let active = app.sessions.iter().filter(|s| s.is_active).count();
        assert_eq!(active, 1, "exactly one active session");
        assert!(
            app.sessions
                .iter()
                .find(|s| s.full_id == "full-0")
                .unwrap()
                .is_active,
            "resumed entry must be the active one"
        );
        assert!(
            !app.sessions
                .iter()
                .find(|s| s.full_id == "full-1")
                .unwrap()
                .is_active,
            "others must be inactive"
        );
    }

    #[test]
    fn add_new_session_pushes_truly_new_id() {
        // A brand-new id (e.g. /new, /clear) still appends a fresh active row.
        let mut app = app_with_sessions(2);
        app.add_new_session("new-id-99".to_string());
        assert_eq!(app.sessions.len(), 3);
        assert_eq!(app.session_id, "new-id-99");
        assert!(
            app.sessions
                .iter()
                .find(|s| s.full_id == "new-id-99")
                .unwrap()
                .is_active
        );
    }
}
