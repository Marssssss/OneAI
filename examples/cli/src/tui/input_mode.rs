//! Input mode definitions for the TUI.
//!
//! Supports two modes:
//! - SingleLine: simple input, Enter sends
//! - MultiLineVim: vim-style multi-line editor (future phase)

/// The current input mode of the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Single-line input mode — Enter sends, Ctrl+Enter inserts newline.
    #[default]
    SingleLine,
    /// Multi-line vim-style editor mode (future implementation).
    MultiLineVim {
        cursor_position: usize,
        mode: VimMode,
    },
}

/// Vim mode for multi-line editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    Normal,
    #[default]
    Insert,
}
