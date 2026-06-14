//! Spinner animation frames for the TUI.
//!
//! Provides a Braille-pattern spinner animation for "thinking" state
//! and other async indicators.

/// Spinner frames using Braille patterns.
pub const SPINNER_FRAMES: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// Get the current spinner character based on the frame index.
pub fn spinner_char(frame: usize) -> &'static str {
    SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
}

/// Advance the spinner frame counter.
pub fn advance_frame(frame: usize) -> usize {
    (frame + 1) % SPINNER_FRAMES.len()
}
