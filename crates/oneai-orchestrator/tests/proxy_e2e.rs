//! End-to-end control-plane + WS reverse-proxy test WITHOUT docker:
//! a real tokio-tungstenite echo server stands in for the session
//! container; the FakeRunner pins the session's host port to it.

mod common;

use std::net::SocketAddr;

use common::{test_state, TEST_SECRET};
use futures::{SinkExt, StreamExt};
use oneai_orchestrator::fsm::SessionState;
use oneai_orchestrator::runner::ContainerRunner as _;
use tokio_tungstenite::tungstenite::Message;

/// Spawn a ws echo server; returns its address and a shutdown-safe handle
/// (runs until the test process ends).
async fn spawn_echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if ws.send(Message::Text(t)).await.is_err() {
                                break;
                            }
                        }
                        Ok(Message::Binary(b)) => {
                            if ws.send(Message::Binary(b)).await.is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
            });
        }
    });
    addr
}

/// Serve the control-plane router on an ephemeral port.
async fn spawn_control_plane(
    state: std::sync::Arc<oneai_orchestrator::server::OrchestratorState>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, oneai_orchestrator::routes::router(state))
            .await
            .ok();
    });
    addr
}

fn http() -> reqwest::Client {
    // no_proxy: dev machines may export HTTP(S)_PROXY — never route
    // loopback test traffic through it.
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn auth_header() -> (String, String) {
    ("Authorization".to_string(), format!("Bearer {TEST_SECRET}"))
}

#[tokio::test]
async fn full_proxy_chain_and_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;
    let echo_addr = spawn_echo_server().await;
    runner.set_fixed_port("echo", echo_addr.port()).await;
    let cp = spawn_control_plane(st.clone()).await;
    let base = format!("http://{cp}");

    // ── 1. POST /v1/sessions (authed) → 201 Running ──
    let resp = http()
        .post(format!("{base}/v1/sessions"))
        .header(auth_header().0, auth_header().1)
        .json(&serde_json::json!({ "session_id": "echo" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["session"]["state"], "Running");
    assert_eq!(body["ws_url"], "/v1/sessions/echo/ws");
    assert_eq!(body["session"]["host_port"], echo_addr.port());

    // ── 2. Unauthenticated POST → 401 ──
    let resp = http()
        .post(format!("{base}/v1/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // ── 3. GET list / get single ──
    let resp = http()
        .get(format!("{base}/v1/sessions"))
        .header(auth_header().0, auth_header().1)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
    let resp = http()
        .get(format!("{base}/v1/sessions/echo"))
        .header(auth_header().0, auth_header().1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = http()
        .get(format!("{base}/v1/sessions/nope"))
        .header(auth_header().0, auth_header().1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // ── 4. healthz needs no auth ──
    let resp = http().get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // ── 5. WS proxy: wrong token → 401 handshake rejection ──
    let bad_url = format!("ws://{cp}/v1/sessions/echo/ws?token=wrong");
    let err = tokio_tungstenite::connect_async(&bad_url)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("401"),
        "expected 401 handshake, got: {err}"
    );

    // ── 6. WS proxy: token query auth → echo round-trip ──
    let url = format!("ws://{cp}/v1/sessions/echo/ws?token={TEST_SECRET}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    ws.send(Message::Text("hello mvs2".into())).await.unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("echo timeout")
        .unwrap()
        .unwrap();
    match echoed {
        Message::Text(t) => assert_eq!(t.as_str(), "hello mvs2"),
        other => panic!("expected Text, got {other:?}"),
    }
    // Binary round-trip too.
    ws.send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    let echoed = ws.next().await.unwrap().unwrap();
    assert!(matches!(echoed, Message::Binary(ref b) if b.as_ref() == [1, 2, 3]));

    // active_conns is now 1.
    let entry = st.table.get("echo").await.unwrap();
    assert_eq!(
        entry
            .active_conns
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    // ── 7. Disconnect → guard drops active_conns ──
    ws.close(None).await.ok();
    drop(ws);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let entry = st.table.get("echo").await.unwrap();
    assert_eq!(
        entry
            .active_conns
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // ── 8. Hibernate, then WS connect auto-resumes (D6) ──
    st.table
        .cas_transition(
            "echo",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let handle = st.table.get("echo").await.unwrap().handle.clone().unwrap();
    runner.stop(&handle).await.unwrap();

    let (mut ws2, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect after hibernate (auto-resume)");
    ws2.send(Message::Text("resumed!".into())).await.unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), ws2.next())
        .await
        .expect("echo timeout after resume")
        .unwrap()
        .unwrap();
    match echoed {
        Message::Text(t) => assert_eq!(t.as_str(), "resumed!"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert!(runner.count_calls("start:").await >= 1);
    assert_eq!(
        st.table.get("echo").await.unwrap().state,
        SessionState::Running
    );
    ws2.close(None).await.ok();

    // ── 9. DELETE destroys + de-lists ──
    let resp = http()
        .delete(format!("{base}/v1/sessions/echo"))
        .header(auth_header().0, auth_header().1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["deleted"], "echo");
    assert!(st.table.get("echo").await.is_none());
    assert!(runner
        .call_log()
        .await
        .iter()
        .any(|c| c == "destroy:oneai-orch-echo:true"));
}
