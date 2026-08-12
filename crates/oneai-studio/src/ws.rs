//! WebSocket handler — real-time event streaming to Studio frontend.
//!
//! Each WS connection subscribes to the engine bus's `EngineYield` stream
//! (set on `StudioState` via `set_bus` when the CLI attaches a runner) and
//! forwards each yield as a newline-terminated `serialize_yield` JSON line.
//! With no bus attached (standalone `serve()`), the socket idles after the
//! welcome message.

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};

use crate::state::StudioState;

// ─── WebSocket Upgrade ──────────────────────────────────────────────

/// Handler for WebSocket upgrade requests at `/ws`.
///
/// The client connects to this endpoint to receive real-time `EngineYield`
/// events (iteration start, tool calls, paradigm switches, streaming chunks,
/// …) pushed from the engine bus the runner emits to.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<std::sync::Arc<StudioState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

// ─── Socket Handler ─────────────────────────────────────────────────

/// Handle an established WebSocket connection.
///
/// Subscribes to the bus's yield stream (if a runner is attached) and
/// forwards each `EngineYield` as a JSON text message. Also reads incoming
/// messages from the client (e.g. "ping") for future extension.
async fn handle_socket(socket: WebSocket, state: std::sync::Arc<StudioState>) {
    let (mut sender, mut receiver) = socket.split();

    // Send initial connection message.
    let welcome = serde_json::json!({
        "type": "connected",
        "message": "OneAI Studio WebSocket connected"
    });
    let welcome_str = serde_json::to_string(&welcome).unwrap_or_default();
    let _ = sender.send(Message::from(welcome_str)).await;

    // Subscribe to the engine bus's yield stream, if a runner is attached.
    let rx_opt = state.subscribe().await;

    let send_task = tokio::spawn(async move {
        if let Some(mut rx) = rx_opt {
            while let Ok(yield_) = rx.recv().await {
                let line = oneai_bus::serialize_yield(&yield_).unwrap_or_default();
                if sender.send(Message::from(line)).await.is_err() {
                    break; // Client disconnected.
                }
            }
        }
        // No bus attached (standalone server) → nothing to forward; the
        // send task ends and the socket waits on the recv task below.
    });

    // Read incoming messages from the client (for future extension).
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    tracing::debug!("Studio WS received: {}", text);
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}

#[cfg(test)]
mod tests {
    use oneai_bus::{EngineBus, EngineYield, InProcessBus};
    use std::sync::Arc;

    #[test]
    fn serialize_yield_all_kinds() {
        let yields = vec![
            EngineYield::IterationStart {
                turn_id: "t".into(),
                iteration: 1,
                paradigm: oneai_bus::BusParadigmKind::ReAct,
            },
            EngineYield::DirectAnswer {
                turn_id: "t".into(),
                text: "hello".into(),
            },
            EngineYield::StreamChunk {
                turn_id: "t".into(),
                text: "chunk".into(),
            },
            EngineYield::Error {
                recoverable: false,
                message: "oops".into(),
            },
        ];
        for y in &yields {
            let line = oneai_bus::serialize_yield(y).unwrap();
            assert!(line.ends_with('\n'));
            let back = oneai_bus::parse_yield(line.trim()).unwrap();
            let line2 = oneai_bus::serialize_yield(&back).unwrap();
            assert_eq!(line, line2);
        }
    }

    #[tokio::test]
    async fn bus_emit_reaches_subscriber() {
        let bus: Arc<dyn EngineBus> = Arc::new(InProcessBus::default());
        let mut rx = bus.subscribe_yields();
        bus.emit(EngineYield::StreamChunk {
            turn_id: "t".into(),
            text: "hi".into(),
        })
        .unwrap();
        assert!(matches!(
            rx.recv().await.unwrap(),
            EngineYield::StreamChunk { .. }
        ));
    }
}
