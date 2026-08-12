//! Wire bridge — bind an arbitrary async byte stream to an [`InProcessBus`].
//!
//! The sidecar (`oneai serve`) accepts an IPC connection ([`IpcStream`] from
//! `oneai-supervisor`, or a `tokio::io::duplex` in tests) and runs
//! [`bridge_connection`] on it. The bridge is the sidecar's per-connection
//! glue: it pipes the two bus channels across the wire —
//!
//! - **yield forwarder**: `bus.subscribe_yields()` → serialize each yield as
//!   one JSON line → write to the stream.
//! - **directive reader**: read one JSON line → parse a [`Directive`] →
//!   `bus.submit(directive)` (the bus resolves `Approve`/`Interrupt` itself;
//!   user directives land on the engine driver's stream).
//!
//! The two halves run concurrently on the same connection; when either ends
//! (client disconnect = reader EOF, engine shutdown = yield stream closed, or
//! a write error) the other is cancelled. The bridge is variant-agnostic — it
//! does not inspect [`Directive`] / [`EngineYield`] payloads, so approval
//! correlation (`request_id`) and interrupt (the bus's registered cancel token)
//! work unchanged over the wire.
//!
//! Transport-agnostic by design: `S: AsyncRead + AsyncWrite + Unpin + Send`.
//! The concrete `IpcStream` binding happens at the CLI; this module stays below
//! `oneai-supervisor` and `oneai-app`.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::bus::{EngineBus, InProcessBus};
use crate::protocol::EngineYield;
use crate::wire::{parse_directive, serialize_yield};
use crate::Result as BusResult;

/// Bridge one connection to an [`InProcessBus`] until either end closes.
///
/// Returns `Ok(())` on a clean client disconnect (reader EOF) or engine
/// shutdown (yield stream closed); `Err` only on a write failure (the client
/// stopped reading mid-yield). Malformed directive lines and unresolvable
/// directives are surfaced back to the frontend as [`EngineYield::Error`] on
/// the same bus — they do not tear down the connection.
pub async fn bridge_connection<S>(stream: S, bus: Arc<InProcessBus>) -> BusResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, write) = tokio::io::split(stream);
    let reader = BufReader::new(read);

    // Subscribe the yield forwarder HERE — before the reader runs and before
    // the forwarder task is polled — so no `bus.emit` (from `read_directives`
    // on a malformed line, or from the engine) can race ahead of the
    // subscription and be dropped on a zero-receiver broadcast. The receiver
    // moves into the forwarder task; the writer half moves with it.
    let rx = bus.subscribe_yields();
    let mut forwarder = tokio::spawn(forward_yields(write, rx));
    let reader_fut = read_directives(reader, bus);
    tokio::pin!(reader_fut);

    // Either half ending ends the bridge. If the reader finished (client EOF)
    // the forwarder is still parked on `rx.recv()` — abort it. If the forwarder
    // finished (yield stream closed / write error) the reader future is dropped
    // (cancelling its in-flight `read_line`).
    tokio::select! {
        _ = &mut forwarder => {},
        _ = &mut reader_fut => {},
    }
    forwarder.abort();
    Ok(())
}

/// Drain the bus's yield broadcast and write each yield as one JSON line.
///
/// The `rx` receiver is created by the caller ([`bridge_connection`]) BEFORE
/// the reader runs, so no emitted yield is lost to a subscription race.
async fn forward_yields<W>(
    mut write: W,
    mut rx: tokio::sync::broadcast::Receiver<EngineYield>,
) -> BusResult<()>
where
    W: AsyncWrite + Unpin + Send,
{
    loop {
        match rx.recv().await {
            Ok(yield_) => {
                let line = serialize_yield(&yield_)?;
                write.write_all(line.as_bytes()).await?;
            }
            // Lagging subscribers miss events with a count — skip and keep
            // reading; the frontend already lost those yields and will recover
            // from the next snapshot it cares about.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_n)) => continue,
            // Yield stream closed — engine shut down. Flush any buffered
            // writes (the final yields) then return.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let _ = write.flush().await;
                return Ok(());
            }
        }
    }
}

/// Read newline-delimited directive lines and submit each to the bus.
async fn read_directives<R>(mut reader: BufReader<R>, bus: Arc<InProcessBus>)
where
    R: AsyncRead + Unpin + Send,
{
    let mut line = String::new();
    loop {
        line.clear();
        // read_line appends including the trailing `\n`; returns 0 on EOF.
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(_) => return, // ungraceful client → drop the connection
        };
        if n == 0 {
            return; // clean client disconnect
        }
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.is_empty() {
            continue;
        }
        let directive = match parse_directive(trimmed) {
            Ok(d) => d,
            Err(e) => {
                let _ = bus.emit(EngineYield::Error {
                    recoverable: true,
                    message: format!("malformed directive line: {e}"),
                });
                continue;
            }
        };
        if let Err(e) = bus.submit(directive).await {
            // e.g. an `Approve` for an unknown / already-resolved request_id.
            let _ = bus.emit(EngineYield::Error {
                recoverable: true,
                message: format!("directive rejected: {e}"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BusTurnSummary, Directive};
    use oneai_core::{ContentBlock, InteractionRequest, InteractionResponse};

    /// Build a `Directive` and serialize it to one `\n`-terminated line.
    fn dir_line(d: &Directive) -> String {
        let mut s = serde_json::to_string(d).expect("serialize directive");
        s.push('\n');
        s
    }

    /// A fake engine driver: drains the directive stream the bus forwards to,
    /// and on each `UserMessage` emits a `StreamChunk` + `TurnComplete`. Lets
    /// the bridge test run without an `AppSession`.
    fn spawn_fake_driver(
        mut directive_rx: tokio::sync::mpsc::Receiver<Directive>,
        bus: Arc<InProcessBus>,
        turn_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            while let Some(directive) = directive_rx.recv().await {
                if let Directive::UserMessage { content } = directive {
                    let task: String = content
                        .into_iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    let _ = bus.emit(EngineYield::StreamChunk {
                        turn_id: turn_id.clone(),
                        text: format!("echo: {task}"),
                    });
                    let _ = bus.emit(EngineYield::TurnComplete {
                        turn_id: turn_id.clone(),
                        summary: BusTurnSummary {
                            final_answer: format!("echo: {task}"),
                            iterations: 1,
                            completed: true,
                            active_paradigm: crate::BusParadigmKind::ReAct,
                        },
                    });
                }
            }
        })
    }

    /// A connected client pair: a persistent line-reader + a writer over the
    /// same duplex, so read-ahead buffering never loses a line.
    struct Client {
        reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    }

    impl Client {
        fn new(stream: tokio::io::DuplexStream) -> Self {
            let (read, write) = tokio::io::split(stream);
            Self {
                reader: BufReader::new(read),
                writer: write,
            }
        }

        async fn send(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn recv_line(&mut self) -> String {
            let mut buf = String::new();
            self.reader.read_line(&mut buf).await.expect("read_line");
            buf
        }
    }

    #[tokio::test]
    async fn roundtrip_user_message_to_yields() {
        let (client_stream, server) = tokio::io::duplex(8 * 1024);
        let (bus, directive_rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let _driver = spawn_fake_driver(directive_rx, bus.clone(), "t1");
        let bridge = tokio::spawn(bridge_connection(server, bus.clone()));

        let mut c = Client::new(client_stream);
        c.send(&dir_line(&Directive::UserMessage {
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }))
        .await;

        let first = c.recv_line().await;
        assert!(first.contains("stream_chunk"), "got: {first}");
        assert!(first.contains("echo: hello"), "got: {first}");
        let second = c.recv_line().await;
        assert!(second.contains("turn_complete"), "got: {second}");

        drop(c);
        let _ = bridge.await;
    }

    #[tokio::test]
    async fn approval_roundtrips_over_wire() {
        let (client_stream, server) = tokio::io::duplex(8 * 1024);
        let (bus, directive_rx) = InProcessBus::new();
        let bus = Arc::new(bus);

        // Fake driver: on UserMessage, request approval, then emit the
        // decision as a DirectAnswer so the test observes the resolve.
        let bus_for_driver = bus.clone();
        let driver = tokio::spawn(async move {
            let mut rx = directive_rx;
            while let Some(d) = rx.recv().await {
                if matches!(d, Directive::UserMessage { .. }) {
                    let resp = bus_for_driver
                        .request_approval(InteractionRequest::NetworkApproval {
                            host: "example.com".to_string(),
                            requested_by: "test".to_string(),
                        })
                        .await
                        .expect("approval resolved");
                    let _ = bus_for_driver.emit(EngineYield::DirectAnswer {
                        turn_id: "t2".to_string(),
                        text: format!("{resp:?}"),
                    });
                }
            }
        });

        let bridge = tokio::spawn(bridge_connection(server, bus.clone()));
        let mut c = Client::new(client_stream);

        // 1. UserMessage → driver calls request_approval → bridge forwards
        //    ApprovalRequest.
        c.send(&dir_line(&Directive::UserMessage {
            content: vec![ContentBlock::Text { text: "go".into() }],
        }))
        .await;

        // 2. Read ApprovalRequest, extract request_id.
        let approval_line = c.recv_line().await;
        assert!(
            approval_line.contains("approval_request"),
            "got: {approval_line}"
        );
        let approval: serde_json::Value = serde_json::from_str(approval_line.trim()).unwrap();
        let request_id = approval["request_id"].as_str().unwrap().to_string();

        // 3. Send Directive::Approve with that request_id (Proceed = unit
        //    variant, externally-tagged → bare "Proceed").
        c.send(&dir_line(&Directive::Approve {
            request_id,
            response: InteractionResponse::Proceed,
        }))
        .await;

        // 4. Driver's request_approval resolves → it emits DirectAnswer.
        let answer = c.recv_line().await;
        assert!(answer.contains("direct_answer"), "got: {answer}");

        driver.abort();
        drop(c);
        let _ = bridge.await;
    }

    #[tokio::test]
    async fn interrupt_cancels_registered_token() {
        let (client_stream, server) = tokio::io::duplex(8 * 1024);
        let (bus, directive_rx) = InProcessBus::new();
        let bus = Arc::new(bus);

        // Register a cancel token the way the engine driver would at turn start.
        let token = tokio_util::sync::CancellationToken::new();
        bus.register_interrupt(token.clone());
        // Drain forwarded directives so the bus's channel doesn't back up.
        let _drain = tokio::spawn(async move {
            let mut rx = directive_rx;
            while rx.recv().await.is_some() {}
        });

        let bridge = tokio::spawn(bridge_connection(server, bus.clone()));
        let mut c = Client::new(client_stream);
        c.send(&dir_line(&Directive::Interrupt {
            reason: oneai_core::InterruptReason::Custom {
                reason: "user_stop".to_string(),
            },
        }))
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            token.is_cancelled(),
            "interrupt directive did not cancel the token"
        );

        drop(c);
        let _ = bridge.await;
    }

    #[tokio::test]
    async fn eof_disconnect_is_clean() {
        let (client_stream, server) = tokio::io::duplex(8 * 1024);
        let (bus, _directive_rx) = InProcessBus::new();
        let bus = Arc::new(bus);

        let bridge = tokio::spawn(bridge_connection(server, bus.clone()));
        drop(client_stream); // immediate disconnect
        let result = bridge.await;
        assert!(result.is_ok(), "bridge should return Ok on clean EOF");
    }

    #[tokio::test]
    async fn malformed_directive_emits_error_yield() {
        let (client_stream, server) = tokio::io::duplex(8 * 1024);
        let (bus, _directive_rx) = InProcessBus::new();
        let bus = Arc::new(bus);

        let bridge = tokio::spawn(bridge_connection(server, bus.clone()));
        let mut c = Client::new(client_stream);
        c.send("{ not valid json }\n").await;

        let err_line = c.recv_line().await;
        assert!(err_line.contains("error"), "got: {err_line}");
        assert!(err_line.contains("malformed"), "got: {err_line}");

        drop(c);
        let _ = bridge.await;
    }
}
