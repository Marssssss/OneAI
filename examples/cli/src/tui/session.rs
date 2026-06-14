//! Session state management for the TUI.
//!
//! Holds shared resources for the TUI session (App, AppSession).

use std::sync::Arc;

/// Holds shared resources for the TUI session.
pub struct SessionState {
    pub app: Arc<oneai_app::App>,
    pub session: oneai_app::AppSession,
}

impl SessionState {
    #[allow(dead_code)]
    pub fn new(app: Arc<oneai_app::App>) -> Self {
        let session = app.create_session();
        Self { app, session }
    }

    pub fn reset_session(&mut self) {
        self.session = self.app.create_session();
    }
}
