//! Color theme definitions for the OneAI TUI.
//!
//! Uses brighter, higher-contrast colors for readability.
//! Replaces ANSI DarkGray (8) with Gray (7) or custom Rgb where needed.

use ratatui::style::Color;

// ─── Brand Colors ──────────────────────────────────────────────────────────

/// Brand character colors for the "OneAI" gradient.
pub const BRAND_O: Color = Color::Rgb(255, 107, 107); // Coral red
pub const BRAND_N: Color = Color::Rgb(78, 205, 196);  // Teal green
pub const BRAND_E: Color = Color::Rgb(69, 183, 209);  // Sky blue
pub const BRAND_A: Color = Color::Rgb(150, 206, 180); // Mint green
pub const BRAND_I: Color = Color::Rgb(255, 234, 167); // Warm gold

/// Brand line background color.
pub const BRAND_BG: Color = Color::Rgb(30, 30, 46); // Dark blue-gray

// ─── Message Colors ────────────────────────────────────────────────────────

/// User message color — bright cyan for readability.
pub const USER_COLOR: Color = Color::Rgb(80, 220, 220); // Bright cyan
/// User message border color.
pub const USER_BORDER: Color = Color::Rgb(60, 190, 190); // Cyan border

/// Assistant message color — bright green for readability.
pub const ASSISTANT_COLOR: Color = Color::Rgb(80, 220, 140); // Bright green
/// Assistant message border color.
pub const ASSISTANT_BORDER: Color = Color::Rgb(50, 180, 100); // Green border

/// Tool call color — bright magenta.
pub const TOOL_CALL_COLOR: Color = Color::Rgb(220, 120, 220); // Bright magenta
/// Tool call border color.
pub const TOOL_CALL_BORDER: Color = Color::Rgb(180, 100, 200); // Magenta border

/// Tool result (success) color — bright blue.
pub const TOOL_RESULT_SUCCESS_COLOR: Color = Color::Rgb(100, 180, 255); // Bright blue
/// Tool result (failure) color — bright red.
pub const TOOL_RESULT_FAILURE_COLOR: Color = Color::Rgb(255, 80, 80); // Bright red

/// Approval card color — bright yellow.
pub const APPROVAL_COLOR: Color = Color::Rgb(255, 220, 80); // Bright yellow
/// Approval border color.
pub const APPROVAL_BORDER: Color = Color::Rgb(220, 180, 40);

/// System message color — gray (brighter than DarkGray).
pub const SYSTEM_COLOR: Color = Color::Gray;

/// Thinking state color — gray (brighter than DarkGray).
pub const THINKING_COLOR: Color = Color::Gray;

/// Error message color — bright red.
pub const ERROR_COLOR: Color = Color::Rgb(255, 80, 80); // Bright red

/// Headline/header color — bright yellow, used for markdown headers and table borders.
pub const HEADLINE_COLOR: Color = Color::Rgb(255, 220, 80); // Bright yellow

// ─── Sidebar Colors ────────────────────────────────────────────────────────

/// Sidebar border color.
pub const SIDEBAR_BORDER: Color = Color::Gray;

/// Sidebar section title color.
pub const SIDEBAR_TITLE_COLOR: Color = Color::Rgb(255, 220, 80); // Bright yellow

/// Sidebar context — provider/model color.
pub const CONTEXT_PROVIDER_COLOR: Color = Color::Rgb(80, 220, 220); // Bright cyan

/// Sidebar context — session ID color — brighter gray.
pub const CONTEXT_SESSION_COLOR: Color = Color::Gray;

/// Inactive session color (for non-active sessions in sidebar).
pub const INACTIVE_SESSION_COLOR: Color = Color::Rgb(120, 120, 120);

/// Sidebar context — paradigm color.
pub const CONTEXT_PARADIGM_COLOR: Color = Color::Rgb(255, 220, 80); // Bright yellow

/// Sidebar context — cost/token color.
pub const CONTEXT_COST_COLOR: Color = Color::Rgb(80, 220, 140); // Bright green

/// Active paradigm indicator color.
pub const ACTIVE_PARADIGM_COLOR: Color = Color::Rgb(255, 220, 80); // Bright yellow

/// Inactive paradigm color — gray.
pub const INACTIVE_PARADIGM_COLOR: Color = Color::Gray;

/// Active tool highlight color.
pub const ACTIVE_TOOL_COLOR: Color = Color::Rgb(80, 220, 140); // Bright green

/// Inactive tool color — gray.
pub const INACTIVE_TOOL_COLOR: Color = Color::Gray;

// ─── Code Block Colors ─────────────────────────────────────────────────────

/// Code block background color.
pub const CODE_BLOCK_BG: Color = Color::Rgb(40, 40, 60); // Dark blue-gray

/// Code block border color.
pub const CODE_BLOCK_BORDER: Color = Color::Gray;

/// Code block language label color.
pub const CODE_LANG_COLOR: Color = Color::Rgb(200, 200, 180); // Light warm gray

// ─── Diff Colors ───────────────────────────────────────────────────────────

/// Diff added line background.
pub const DIFF_ADDED_BG: Color = Color::Rgb(22, 78, 22); // Dark green BG

/// Diff deleted line background.
pub const DIFF_DELETED_BG: Color = Color::Rgb(52, 20, 20); // Dark red BG

// ─── Progress Bar Colors ───────────────────────────────────────────────────

/// Progress bar fill color.
pub const PROGRESS_FILL: Color = Color::Rgb(80, 220, 140); // Bright green

/// Progress bar background color.
pub const PROGRESS_BG: Color = Color::Gray;

// ─── Input Colors ──────────────────────────────────────────────────────────

/// Input prompt color.
pub const INPUT_PROMPT_COLOR: Color = Color::Rgb(80, 220, 220); // Bright cyan

/// Input text color.
pub const INPUT_TEXT_COLOR: Color = Color::White;

/// Input border color.
pub const INPUT_BORDER: Color = Color::Gray;

/// Input hint text color — gray (brighter).
pub const INPUT_HINT_COLOR: Color = Color::Rgb(160, 160, 160); // Medium gray

/// Input cursor color.
pub const INPUT_CURSOR_COLOR: Color = Color::White;

// ─── Scrollbar Colors ──────────────────────────────────────────────────────

/// Scrollbar track color.
pub const SCROLLBAR_TRACK: Color = Color::Rgb(60, 60, 80); // Dark but visible

/// Scrollbar thumb color.
pub const SCROLLBAR_THUMB: Color = Color::Rgb(150, 150, 170); // Bright gray
