//! Bus consumer — the TUI's `DirectiveRuntime` impl + a thin spawn wrapper.
//!
//! The shared dispatch lives in `oneai_app::directive_pump` (extracted from
//! here) so the sidecar (`oneai serve`) drives the engine with the *same*
//! logic — zero drift. This module:
//!
//! - impls [`DirectiveRuntime`] for [`SessionState`] (TUI holds it as
//!   `Arc<Mutex<SessionState>>`; the pump locks per directive, as before).
//! - re-exports the shared bus↔agent DTO conversions (`process_yield` reads
//!   them).
//! - wraps [`oneai_app::spawn_directive_pump`] so the TUI call site keeps its
//!   `Arc<Mutex<SessionState>>` signature.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_agent::{AgentLoop, ParadigmKind};
use oneai_app::DirectiveRuntime;
use oneai_bus::{Directive, InProcessBus};
use oneai_core::error::Result;
use oneai_core::{traits::LlmProvider, Message};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

// Re-export the shared conversions so `process_yield` (and any other TUI
// reader of bus DTOs) imports them from the same place the sidecar does.
pub use oneai_app::directive_pump::{
    paradigm_from_bus, sub_agent_kind_from_bus, sub_agent_summary_from_bus, tool_call_from_bus,
};

use super::session::SessionState;
use oneai_app::CompactOutcome;

#[async_trait]
impl DirectiveRuntime for SessionState {
    async fn run_turn(
        &mut self,
        task: &str,
        interrupt_slot: Arc<Mutex<Option<AgentLoop>>>,
    ) -> Result<oneai_bus::BusTurnSummary> {
        self.session.run_turn_via_bus(task, interrupt_slot).await
    }

    async fn set_paradigm(&mut self, to: ParadigmKind) -> Option<ParadigmKind> {
        self.session.set_paradigm(to)
    }

    async fn set_plan_mode(&mut self, on: bool) {
        self.session.set_plan_mode(on);
    }

    async fn compact(&mut self, keep_recent_turns: usize) -> Result<CompactOutcome> {
        self.session.compact(keep_recent_turns).await
    }

    fn provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.session.provider().cloned()
    }

    async fn create_session(&mut self, id: Option<String>) -> String {
        match id {
            Some(wanted) => {
                let new = self.app.create_session_with_id(&wanted).await;
                let nid = new.session_id().to_string();
                self.session = new;
                nid
            }
            // None ⇒ fresh uuid; reset_session swaps the AppSession in place.
            None => {
                SessionState::reset_session(self);
                self.session.session_id().to_string()
            }
        }
    }

    async fn load_session(&mut self, id: String) -> (String, Vec<Message>) {
        let sessions = self.app.list_conversations().await;
        let resolved = if sessions.iter().any(|s| s.id == id) {
            id.clone()
        } else {
            let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
            match matches.len() {
                1 => matches[0].id.clone(),
                _ => id.clone(),
            }
        };
        let new = self.app.create_session_with_id(&resolved).await;
        let msgs = new.conversation().messages.clone();
        self.session = new;
        (resolved, msgs)
    }

    async fn reset_session(&mut self) -> String {
        SessionState::reset_session(self);
        self.session.session_id().to_string()
    }

    async fn delete_session(&mut self, id: String) -> Result<()> {
        self.app.delete_conversation(&id).await
    }

    async fn session_id(&mut self) -> String {
        self.session.session_id().to_string()
    }
}

/// Thin wrapper over the shared [`oneai_app::spawn_directive_pump`], pinned to
/// the TUI's `Arc<Mutex<SessionState>>` holder. The emit of `EngineYield::*`
/// (including turn errors) happens inside the shared pump; nothing here.
#[allow(clippy::future_not_send)]
pub fn spawn_directive_pump(
    directive_rx: mpsc::Receiver<Directive>,
    session_state: Arc<Mutex<SessionState>>,
    interrupt_slot: Arc<Mutex<Option<AgentLoop>>>,
    bus: Arc<InProcessBus>,
) -> JoinHandle<()> {
    oneai_app::spawn_directive_pump(directive_rx, session_state, interrupt_slot, bus)
}
