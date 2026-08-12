//! Integration tests for the oneai-bus protocol + in-process bus + wire codec.

use oneai_bus::{
    parse_directive, parse_yield, serialize_directive, serialize_yield, BusToolCall,
    Directive as BusDirective, EngineBus, EngineYield, InProcessBus,
};
use oneai_core::{ContentBlock, InteractionRequest, InteractionResponse, InterruptReason};

use tokio_util::sync::CancellationToken;

/// serde round-trip for a representative inbound directive.
#[tokio::test]
async fn directive_user_message_round_trips() {
    let d = BusDirective::UserMessage {
        content: vec![ContentBlock::Text {
            text: "hello bus".to_string(),
        }],
    };
    let line = serialize_directive(&d).unwrap();
    assert!(line.ends_with('\n'));
    let back = parse_directive(line.trim()).unwrap();
    assert_eq!(format!("{back:?}"), format!("{d:?}"));
}

/// serde round-trip for a representative outbound yield with a nested DTO.
#[tokio::test]
async fn yield_tool_calls_round_trips() {
    let y = EngineYield::ToolCalls {
        turn_id: "t_1".to_string(),
        calls: vec![BusToolCall {
            id: "c_1".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"cmd": "ls"}),
        }],
    };
    let line = serialize_yield(&y).unwrap();
    let back: EngineYield = parse_yield(line.trim()).unwrap();
    assert_eq!(format!("{back:?}"), format!("{y:?}"));
}

/// Malformed input surfaces a codec error, not a panic.
#[tokio::test]
async fn malformed_wire_frame_is_a_codec_error() {
    assert!(parse_directive("not json").is_err());
    assert!(parse_yield("{ kind: \"bogus\" }").is_err());
}

/// emit → subscribe_yields delivers to a subscriber.
#[tokio::test]
async fn emit_reaches_subscriber() {
    let (bus, _rx) = InProcessBus::new();
    let mut sub = bus.subscribe_yields();
    bus.emit(EngineYield::TurnStart {
        turn_id: "t_1".to_string(),
        task: "demo".to_string(),
    })
    .unwrap();
    let received = sub.recv().await.unwrap();
    assert!(matches!(received, EngineYield::TurnStart { .. }));
}

/// submit(UserMessage) forwards to the engine driver's directive stream.
#[tokio::test]
async fn submit_user_message_forwards_to_driver() {
    let (bus, mut directive_rx) = InProcessBus::new();
    bus.submit(BusDirective::UserMessage {
        content: vec![ContentBlock::Text {
            text: "go".to_string(),
        }],
    })
    .await
    .unwrap();
    let d = directive_rx.recv().await.unwrap();
    assert!(matches!(d, BusDirective::UserMessage { .. }));
}

/// submit(Approve) for an unknown id is NotAcceptable — no silent drop.
#[tokio::test]
async fn approve_unknown_id_is_rejected() {
    let (bus, _rx) = InProcessBus::new();
    let err = bus
        .submit(BusDirective::Approve {
            request_id: "nope".to_string(),
            response: InteractionResponse::Proceed,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no pending approval"));
}

/// The full approval round-trip: request_approval blocks → subscriber sees
/// ApprovalRequest → submit(Approve) → request resolves with the response.
#[tokio::test]
async fn approval_request_resolves_via_directive() {
    let (bus, _rx) = InProcessBus::new();
    let bus = std::sync::Arc::new(bus);

    // Subscribe BEFORE requesting so we observe the ApprovalRequest.
    let mut sub = bus.subscribe_yields();

    // Engine side asks for approval (blocks).
    let bus_clone = bus.clone();
    let approval_task = tokio::spawn(async move {
        bus_clone
            .request_approval(InteractionRequest::NetworkApproval {
                host: "example.com".to_string(),
                requested_by: "test".to_string(),
            })
            .await
    });

    // Frontend observes the request, harvests request_id, replies.
    let request_id = match sub.recv().await.unwrap() {
        EngineYield::ApprovalRequest { request_id, .. } => request_id,
        other => panic!("expected ApprovalRequest, got {other:?}"),
    };
    bus.submit(BusDirective::Approve {
        request_id,
        response: InteractionResponse::Proceed,
    })
    .await
    .unwrap();

    let response = approval_task.await.unwrap().unwrap();
    assert!(matches!(response, InteractionResponse::Proceed));
}

/// submit(Interrupt) fires the engine's registered CancellationToken.
#[tokio::test]
async fn interrupt_fires_registered_token() {
    let (bus, _rx) = InProcessBus::new();
    let token = CancellationToken::new();
    bus.register_interrupt(token.clone());
    assert!(!token.is_cancelled());
    bus.submit(BusDirective::Interrupt {
        reason: InterruptReason::Custom {
            reason: "user cancel".to_string(),
        },
    })
    .await
    .unwrap();
    assert!(token.is_cancelled());
}
