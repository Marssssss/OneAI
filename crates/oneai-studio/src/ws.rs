//! WebSocket handler — real-time event streaming to Studio frontend.

use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade, Message},
    extract::State,
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};

use crate::state::{StudioState, StudioEvent};

// ─── WebSocket Upgrade ──────────────────────────────────────────────

/// Handler for WebSocket upgrade requests at `/ws`.
///
/// The client connects to this endpoint to receive real-time events
/// (iteration start, tool calls, paradigm switches, etc.) pushed from
/// the StudioState broadcast channel.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<std::sync::Arc<StudioState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

// ─── Socket Handler ─────────────────────────────────────────────────

/// Handle an established WebSocket connection.
///
/// Reads from the broadcast channel and forwards events to the client
/// as JSON text messages. Also reads incoming messages from the client
/// (e.g., commands like "subscribe", "ping") for future extension.
async fn handle_socket(socket: WebSocket, state: std::sync::Arc<StudioState>) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to the broadcast channel
    let mut rx = state.subscribe();

    // Send initial connection message
    let welcome = serde_json::json!({
        "type": "connected",
        "message": "OneAI Studio WebSocket connected"
    });
    let welcome_str = serde_json::to_string(&welcome).unwrap_or_default();
    let _ = sender.send(Message::from(welcome_str)).await;

    // Forward broadcast events to the client
    let send_task = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            let json = serde_json::to_string(&event).unwrap_or_default();
            if sender.send(Message::from(json)).await.is_err() {
                break; // Client disconnected
            }
        }
    });

    // Read incoming messages from the client (for future extension)
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    // Handle incoming commands (future extension)
                    tracing::debug!("Studio WS received: {}", text);
                    // For now, just log — could add "ping/pong", "subscribe_session", etc.
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    });

    // Wait for either task to finish (client disconnect or broadcast end)
    let _ = tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_studio_event_serialization_all_types() {
        // Verify all StudioEvent variants serialize correctly
        let events = vec![
            StudioEvent::IterationStart { iteration: 1, paradigm: "react".to_string() },
            StudioEvent::DirectAnswer { text: "hello".to_string() },
            StudioEvent::ToolCalls { calls: vec![crate::state::ToolCallView {
                id: "c1".to_string(), tool_name: "shell".to_string(), args: serde_json::json!({"cmd": "ls"}),
            }] },
            StudioEvent::ToolResult { call_id: "c1".to_string(), tool_name: "shell".to_string(), success: true, output_summary: "OK".to_string() },
            StudioEvent::Delegate { task: "implement".to_string(), agent_type: "coder".to_string() },
            StudioEvent::ParadigmSwitch { paradigm: "plan".to_string() },
            StudioEvent::CheckpointSaved { iteration: 3, checkpoint_id: "cp_3".to_string() },
            StudioEvent::TraceEvent { kind: "Thought".to_string(), name: "agent.thought".to_string(), attributes: serde_json::json!({"msg": "thinking"}) },
            StudioEvent::Thinking { text: "reasoning...".to_string() },
            StudioEvent::StreamChunk { text: "chunk".to_string() },
            StudioEvent::ApprovalRequest { tool_name: "shell".to_string(), args: serde_json::json!({}), risk_level: "High".to_string() },
            StudioEvent::ApprovalResponse { approved: true, reason: "OK".to_string() },
            StudioEvent::LoopComplete { result_summary: "Success".to_string() },
            StudioEvent::Error { message: "oops".to_string() },
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let deserialized: StudioEvent = serde_json::from_str(&json).unwrap();
            // Verify roundtrip
            let json2 = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(json, json2);
        }
    }
}
