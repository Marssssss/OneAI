//! Input mode definitions for the TUI.
//!
//! Single-line input: Enter sends, Ctrl+Enter inserts a newline, Up/Down
//! move the cursor between lines (history on the boundary line when empty),
//! Ctrl+C clears the draft then quits on a second press.

/// The current input mode of the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Single-line input mode — Enter sends, Ctrl+Enter inserts newline.
    #[default]
    SingleLine,
}
