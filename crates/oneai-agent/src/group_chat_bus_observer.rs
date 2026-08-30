//! `GroupChatBusObserver` — the group-chat counterpart of [`BusObserver`].
//!
//! A [`GroupChatObserver`] that emits the same `EngineYield` fragment stream a
//! single-agent `BusObserver` does, but tags every per-member fragment with
//! `speaker: Some(member_id)` and brackets each member's turn with
//! [`EngineYield::SpeakerTurn`]. This is the engine-side half that lets a
//! multi-agent group-chat session ride the unified bus (P4's "extend the
//! protocol to carry group chat") — the in-process 3-symbol pump
//! (`Directive::StartGroupChat` / `GroupUserMessage`) drives a
//! `GroupChatSession` through this observer, so a mobile frontend gets a
//! single yield stream with `speaker`-routed fragments, exactly like the
//! `ChatEventView`-with-speaker surface the macOS uniffi group-chat binding
//! already provides — but over the bus.
//!
//! Mirrors `GroupChatCallbackObserver` (`oneai-uniffi/src/group_chat.rs`) —
//! same `current_speaker` tracking (std `Mutex`; callbacks are sync trait
//! methods firing on the tokio worker thread, lock never held across an
//! `.await`), same minimal surface (fragments + speaker boundary; the
//! lifecycle yields `IterationStart`/`ParadigmSwitch`/`TurnComplete` are
//! left as no-op defaults — the frontend brackets a member by
//! `SpeakerTurn`, not by `TurnComplete`, so multiple members don't each
//! emit a turn-complete).

use std::sync::{Arc, Mutex};

use oneai_bus::{EngineBus, EngineYield};

use crate::agent_loop::{ParadigmKind, ToolCallRequest};
use crate::group_chat::GroupChatObserver;
use crate::{AgentLoopObserver, AgentLoopResult};

/// Bridges a `GroupChatSession`'s `GroupChatObserver` callbacks to the
/// [`EngineBus`] yield stream, tagging each fragment with the speaking member.
///
/// Construct one per group round with a fixed `turn_id`; the pump owns it for
/// the round's lifetime. `on_speaker_turn` (called by `GroupChatSession`
/// before each member's `AgentLoop` runs) sets the current speaker and emits
/// `EngineYield::SpeakerTurn`; subsequent fragment callbacks read it back to
/// tag `StreamChunk`/`Thinking`/`DirectAnswer`/`ToolCalls`/`ToolResult`/
/// `Delegate`/`DelegateComplete`.
pub struct GroupChatBusObserver {
    bus: Arc<dyn EngineBus>,
    turn_id: String,
    /// `std::sync::Mutex` (not tokio): the observer callbacks are synchronous
    /// trait methods firing on the tokio worker thread mid-member-run, and the
    /// lock is never held across an `.await` — only a trivial assign/clone.
    current_speaker: Mutex<String>,
}

impl GroupChatBusObserver {
    /// Construct an observer that emits speaker-tagged yields to `bus`,
    /// keyed to `turn_id`.
    pub fn new(bus: Arc<dyn EngineBus>, turn_id: impl Into<String>) -> Self {
        Self {
            bus,
            turn_id: turn_id.into(),
            current_speaker: Mutex::new(String::new()),
        }
    }

    fn emit(&self, y: EngineYield) {
        // No-op on error: zero subscribers is legitimate; other errors are
        // unactionable from a sync observer callback (trace, don't panic).
        let _ = self.bus.emit(y);
    }

    /// Read the current speaker id (`None` before the first `on_speaker_turn`).
    fn speaker(&self) -> Option<String> {
        let g = self
            .current_speaker
            .lock()
            .expect("current_speaker poisoned");
        if g.is_empty() {
            None
        } else {
            Some(g.clone())
        }
    }
}

impl AgentLoopObserver for GroupChatBusObserver {
    // Lifecycle yields (iteration/paradigm/complete/usage/context/plan/tools)
    // are intentionally left as no-ops: a group round has N members, and the
    // frontend brackets each member by `SpeakerTurn` + speaker-tagged
    // fragments rather than by per-member `TurnComplete`. The member's final
    // answer rides `on_direct_answer` → `DirectAnswer`. (Mirrors
    // `GroupChatCallbackObserver`'s minimal surface.)

    fn on_iteration_start(&self, _: usize, _: ParadigmKind) {}
    fn on_delegate(&self, _: &str, _: &str, _: &crate::sub_agent::SubAgentKind) {}
    fn on_paradigm_switch(&self, _: ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}
    fn on_complete(&self, _: &AgentLoopResult) {}

    fn on_direct_answer(&self, text: &str) {
        self.emit(EngineYield::DirectAnswer {
            turn_id: self.turn_id.clone(),
            text: text.to_string(),
            speaker: self.speaker(),
        });
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        self.emit(EngineYield::ToolCalls {
            turn_id: self.turn_id.clone(),
            calls: calls.iter().map(oneai_bus::BusToolCall::from).collect(),
            speaker: self.speaker(),
        });
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &oneai_core::ToolOutput) {
        self.emit(EngineYield::ToolResult {
            turn_id: self.turn_id.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            output: output.clone(),
            speaker: self.speaker(),
        });
    }

    fn on_stream_chunk(&self, text: &str) {
        self.emit(EngineYield::StreamChunk {
            turn_id: self.turn_id.clone(),
            text: text.to_string(),
            speaker: self.speaker(),
        });
    }

    fn on_thinking(&self, text: &str) {
        self.emit(EngineYield::Thinking {
            turn_id: self.turn_id.clone(),
            text: text.to_string(),
            speaker: self.speaker(),
        });
    }

    fn on_tool_intent(&self, call_id: &str, tool_name: &str) {
        self.emit(EngineYield::ToolIntent {
            turn_id: self.turn_id.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            speaker: self.speaker(),
        });
    }
}

impl GroupChatObserver for GroupChatBusObserver {
    fn on_speaker_turn(&self, speaker: &str) {
        // Trivial critical section — std Mutex, no await held. Fires on the
        // tokio worker thread between member runs.
        *self
            .current_speaker
            .lock()
            .expect("current_speaker poisoned") = speaker.to_string();
        self.emit(EngineYield::SpeakerTurn {
            turn_id: self.turn_id.clone(),
            speaker: speaker.to_string(),
        });
    }
}

// `on_delegate`/`on_delegate_complete` emit speaker-tagged yields too — but
// the trait methods have no-op defaults and group-chat members are lean
// persona loops that don't delegate (see `GroupChatSession` design). Wire
// them only if a future member kind starts delegating; for now the defaults
// are correct.

#[cfg(test)]
mod tests {
    //! A fake bus captures emits; assert `on_speaker_turn` → `SpeakerTurn` +
    //! that subsequent fragments carry the tagged speaker.
    use super::*;
    use oneai_bus::{EngineBus, InProcessBus};
    use oneai_core::ToolOutput;
    use tokio::sync::broadcast;

    fn observed_bus() -> (Arc<InProcessBus>, broadcast::Receiver<EngineYield>) {
        let bus = Arc::new(InProcessBus::default());
        let rx = bus.subscribe_yields();
        (bus, rx)
    }

    #[test]
    fn speaker_turn_then_stream_chunk_carries_speaker() {
        let (bus, mut rx) = observed_bus();
        let obs = GroupChatBusObserver::new(bus as Arc<dyn EngineBus>, "t1");

        obs.on_speaker_turn("member-a");
        obs.on_stream_chunk("hi");
        obs.on_tool_result("c1", "calc", &ToolOutput::default());

        let y1 = rx.try_recv().expect("SpeakerTurn emitted");
        match y1 {
            EngineYield::SpeakerTurn { turn_id, speaker } => {
                assert_eq!(turn_id, "t1");
                assert_eq!(speaker, "member-a");
            }
            _ => panic!("expected SpeakerTurn, got {y1:?}"),
        }
        match rx.try_recv().expect("StreamChunk emitted") {
            EngineYield::StreamChunk { text, speaker, .. } => {
                assert_eq!(text, "hi");
                assert_eq!(speaker.as_deref(), Some("member-a"));
            }
            _ => panic!("expected StreamChunk"),
        }
        match rx.try_recv().expect("ToolResult emitted") {
            EngineYield::ToolResult {
                tool_name, speaker, ..
            } => {
                assert_eq!(tool_name, "calc");
                assert_eq!(speaker.as_deref(), Some("member-a"));
            }
            _ => panic!("expected ToolResult"),
        }
        assert!(rx.try_recv().is_err(), "no further yields");
    }

    #[test]
    fn fragment_before_speaker_turn_is_none_speaker() {
        // A fragment fired before any on_speaker_turn emits speaker=None (the
        // single-agent-equivalent state — defensive, shouldn't happen in a
        // real group round but must not panic).
        let (bus, mut rx) = observed_bus();
        let obs = GroupChatBusObserver::new(bus as Arc<dyn EngineBus>, "t2");
        obs.on_direct_answer("early");
        match rx.try_recv().unwrap() {
            EngineYield::DirectAnswer { speaker, .. } => assert_eq!(speaker, None),
            _ => panic!("expected DirectAnswer"),
        }
    }

    #[test]
    fn speaker_switch_updates_subsequent_fragments() {
        let (bus, mut rx) = observed_bus();
        let obs = GroupChatBusObserver::new(bus as Arc<dyn EngineBus>, "t3");
        obs.on_speaker_turn("a");
        obs.on_stream_chunk("from-a");
        obs.on_speaker_turn("b");
        obs.on_stream_chunk("from-b");
        match rx.try_recv().unwrap() {
            EngineYield::SpeakerTurn { speaker, .. } => assert_eq!(speaker, "a"),
            _ => panic!(),
        }
        match rx.try_recv().unwrap() {
            EngineYield::StreamChunk { text, speaker, .. } => {
                assert_eq!(text, "from-a");
                assert_eq!(speaker.as_deref(), Some("a"));
            }
            _ => panic!(),
        }
        match rx.try_recv().unwrap() {
            EngineYield::SpeakerTurn { speaker, .. } => assert_eq!(speaker, "b"),
            _ => panic!(),
        }
        match rx.try_recv().unwrap() {
            EngineYield::StreamChunk { text, speaker, .. } => {
                assert_eq!(text, "from-b");
                assert_eq!(speaker.as_deref(), Some("b"));
            }
            _ => panic!(),
        }
    }
}
