//! Color theme definitions for the OneAI TUI.
//!
//! Consolidated to a small, muted semantic palette to keep the interface calm
//! rather than flashy. All named constants below map onto one of seven roles:
//!
//! | Role     | Meaning                          | Used by                       |
//! |----------|----------------------------------|-------------------------------|
//! | ACCENT   | primary highlight (teal)         | user, prompts, active, tools  |
//! | SUCCESS  | positive outcome (sage)          | assistant, success, progress  |
//! | WARNING  | caution / headings (gold)        | approval, titles, paradigm    |
//! | DANGER   | negative outcome (coral)         | errors, failures, diff del    |
//! | TEXT     | primary foreground (light)       | input text, cursor            |
//! | DIM      | secondary (neutral gray)         | borders, system, thinking      |
//! | MUTED    | tertiary (dark gray)             | labels, hints, inactive        |

use ratatui::style::Color;

// ─── Collapse threshold ─────────────────────────────────────────────────────

/// Maximum lines shown in a collapsed (preview) state before an expand button
/// appears. Content with fewer lines than this is rendered in full and is not
/// collapsible. Applied uniformly to tool-result and thinking blocks.
pub const COLLAPSE_THRESHOLD: usize = 5;

// ─── Semantic Palette ───────────────────────────────────────────────────────

pub const ACCENT: Color = Color::Rgb(98, 176, 188); // muted teal
pub const SUCCESS: Color = Color::Rgb(150, 196, 122); // muted sage green
pub const WARNING: Color = Color::Rgb(214, 182, 96); // muted gold
pub const DANGER: Color = Color::Rgb(208, 124, 124); // muted coral red
pub const TEXT: Color = Color::Rgb(222, 222, 224); // light neutral
pub const DIM: Color = Color::Rgb(132, 132, 142); // neutral gray
pub const MUTED: Color = Color::Rgb(96, 96, 108); // dark gray

// ─── Brand Colors ──────────────────────────────────────────────────────────

/// Brand character colors for the "OneAI" gradient (muted to match the palette).
pub const BRAND_O: Color = DANGER;
pub const BRAND_N: Color = ACCENT;
pub const BRAND_E: Color = Color::Rgb(110, 160, 200); // muted blue
pub const BRAND_A: Color = SUCCESS;
pub const BRAND_I: Color = WARNING;

/// Brand line background color.
pub const BRAND_BG: Color = Color::Rgb(30, 30, 46); // Dark blue-gray

// ─── Message Colors ────────────────────────────────────────────────────────

/// User message color.
pub const USER_COLOR: Color = ACCENT;
/// User message border color.
pub const USER_BORDER: Color = DIM;

/// Assistant message color.
pub const ASSISTANT_COLOR: Color = SUCCESS;
/// Assistant message border color.
pub const ASSISTANT_BORDER: Color = DIM;

/// Tool call color (pending indicator).
pub const TOOL_CALL_COLOR: Color = ACCENT;
/// Tool call border color.
pub const TOOL_CALL_BORDER: Color = DIM;

/// Tool result (success) color.
pub const TOOL_RESULT_SUCCESS_COLOR: Color = SUCCESS;
/// Tool result (failure) color.
pub const TOOL_RESULT_FAILURE_COLOR: Color = DANGER;

/// Tool output body color — the actual stdout/stderr-style content of a tool
/// result. Deliberately a cool muted slate, distinct from `ASSISTANT_COLOR`
/// (sage green) so machine output never blends into the model's prose.
/// Success/failure is still conveyed by the status icon + title border; this
/// color only styles the body text.
pub const TOOL_OUTPUT_COLOR: Color = Color::Rgb(148, 166, 184); // muted cool slate

/// Approval card color.
pub const APPROVAL_COLOR: Color = WARNING;
/// Approval border color.
pub const APPROVAL_BORDER: Color = WARNING;

/// System message color.
pub const SYSTEM_COLOR: Color = DIM;

/// Dim label color for separators/hints (Result, collapse, more-lines).
pub const LABEL_DIM: Color = MUTED;

/// Thinking state color.
pub const THINKING_COLOR: Color = DIM;

/// Error message color.
pub const ERROR_COLOR: Color = DANGER;

/// Headline/header color — used for markdown headers and table borders.
pub const HEADLINE_COLOR: Color = WARNING;

// ─── Sidebar Colors ────────────────────────────────────────────────────────

/// Sidebar border color.
pub const SIDEBAR_BORDER: Color = DIM;

/// Sidebar section title color.
pub const SIDEBAR_TITLE_COLOR: Color = WARNING;

/// Sidebar context — provider/model color.
pub const CONTEXT_PROVIDER_COLOR: Color = ACCENT;

/// Sidebar context — session ID color.
pub const CONTEXT_SESSION_COLOR: Color = DIM;

/// Inactive session color (for non-active sessions in sidebar).
pub const INACTIVE_SESSION_COLOR: Color = MUTED;

/// Sidebar context — paradigm color.
pub const CONTEXT_PARADIGM_COLOR: Color = WARNING;

/// Sidebar context — cost/token color.
pub const CONTEXT_COST_COLOR: Color = SUCCESS;

/// Active paradigm indicator color.
pub const ACTIVE_PARADIGM_COLOR: Color = WARNING;

/// Inactive paradigm color.
pub const INACTIVE_PARADIGM_COLOR: Color = DIM;

/// Active tool highlight color.
pub const ACTIVE_TOOL_COLOR: Color = SUCCESS;

/// Inactive tool color.
pub const INACTIVE_TOOL_COLOR: Color = DIM;

// ─── Code Block Colors ─────────────────────────────────────────────────────

/// Code block background color.
pub const CODE_BLOCK_BG: Color = Color::Rgb(34, 34, 46); // Dark blue-gray

/// Code block border color.
pub const CODE_BLOCK_BORDER: Color = DIM;

/// Code block language label color.
pub const CODE_LANG_COLOR: Color = DIM;

// ─── Diff Colors ───────────────────────────────────────────────────────────

/// Diff added line background.
pub const DIFF_ADDED_BG: Color = Color::Rgb(28, 56, 36); // Dark green BG

/// Diff deleted line background.
pub const DIFF_DELETED_BG: Color = Color::Rgb(56, 30, 30); // Dark red BG

/// Diff added line foreground — clear green (programmer convention: `+` is
/// green). RGB so it renders identically across terminals, not subject to the
/// 16-color palette mapping of `Color::Green`.
pub const DIFF_ADDED_FG: Color = Color::Rgb(122, 196, 143); // clear green

/// Diff deleted line foreground — clear red (programmer convention: `-` is
/// red).
pub const DIFF_DELETED_FG: Color = Color::Rgb(216, 118, 118); // clear red

/// Diff file header (`+++` / `---`) foreground — muted cyan, the conventional
/// file-path color in unified diffs.
pub const DIFF_FILE_HEADER_COLOR: Color = Color::Rgb(110, 168, 200); // muted cyan

/// Diff hunk header (`@@ ... @@`) foreground — dimmed, like git renders them.
pub const DIFF_HUNK_COLOR: Color = DIM;

/// Diff line-number gutter foreground.
pub const DIFF_LINENO_COLOR: Color = MUTED;

// ─── Progress Bar Colors ───────────────────────────────────────────────────

/// Progress bar fill color.
pub const PROGRESS_FILL: Color = SUCCESS;

/// Progress bar background color.
pub const PROGRESS_BG: Color = MUTED;

// ─── Input Colors ──────────────────────────────────────────────────────────

/// Input prompt color.
pub const INPUT_PROMPT_COLOR: Color = ACCENT;

/// Input text color.
pub const INPUT_TEXT_COLOR: Color = TEXT;

/// Input border color.
pub const INPUT_BORDER: Color = DIM;

/// Input hint text color.
pub const INPUT_HINT_COLOR: Color = MUTED;

/// Input cursor color.
pub const INPUT_CURSOR_COLOR: Color = TEXT;

// ─── Scrollbar Colors ──────────────────────────────────────────────────────

/// Scrollbar track color.
pub const SCROLLBAR_TRACK: Color = MUTED;

/// Scrollbar thumb color.
pub const SCROLLBAR_THUMB: Color = DIM;
