//! Message history navigation for the TUI input.
//!
//! Stores previously sent user messages and supports ↑↓ navigation.

/// Message history navigation state.
pub struct MessageHistory {
    /// All previously sent messages (in order).
    history: Vec<String>,
    /// Current navigation position (-1 means "new input mode").
    /// When the user navigates up, this increases; when they type new text,
    /// it resets to -1.
    index: isize,
}

impl MessageHistory {
    /// Create a new empty history.
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
            index: -1,
        }
    }

    /// Add a message to the history.
    pub fn push(&mut self, msg: String) {
        if !msg.is_empty() {
            self.history.push(msg);
            self.index = -1; // Reset to "new input" mode
        }
    }

    /// Navigate up (older messages). Returns the message at the new position, or None.
    pub fn navigate_up(&mut self) -> Option<&str> {
        if self.history.is_empty() {
            return None;
        }
        if self.index < (self.history.len() as isize - 1) {
            self.index += 1;
        }
        self.history.get(self.index as usize).map(|s| s.as_str())
    }

    /// Navigate down (newer messages). Returns the message at the new position,
    /// or empty string if we've returned to "new input" mode.
    pub fn navigate_down(&mut self) -> Option<String> {
        if self.index > 0 {
            self.index -= 1;
            Some(self.history[self.index as usize].clone())
        } else if self.index == 0 {
            self.index = -1;
            Some(String::new()) // Back to new input
        } else {
            None // Already in new input mode
        }
    }

    /// Check if we're in "new input" mode (not navigating history).
    #[allow(dead_code)]
    pub fn is_new_input(&self) -> bool {
        self.index == -1
    }

    /// Reset navigation to "new input" mode.
    pub fn reset(&mut self) {
        self.index = -1;
    }
}

impl Default for MessageHistory {
    fn default() -> Self {
        Self::new()
    }
}
