//! End-to-end smoke test for `oneai serve` — the sidecar.
//!
//! Spawns the `oneai serve` subprocess on a temp UDS socket, connects a
//! socket client, and roundtrips provider-free directives (`ClearSession`,
//! `SwitchParadigm`) to prove the real wiring — `IpcListener::bind` → accept
//! loop → `bridge_connection` → shared `spawn_directive_pump` →
//! `SidecarRuntime` — works over an actual socket. (The bridge's own unit
//! tests cover the wire codec; this test covers the assembled sidecar.)
//!
//! A real turn (`UserMessage`) is not exercised here — it needs a provider,
//! and the bridge unit tests already prove the UserMessage→StreamChunk→
//! TurnComplete path. Provider-free directives are enough to validate the
//! sidecar's integration plumbing.

#![cfg(unix)]

use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Send a `Directive` (as a serialized JSON line) and read yields until one
/// matching `kind` arrives (or the deadline passes).
async fn roundtrip<R, W>(
    reader: &mut BufReader<R>,
    writer: &mut W,
    directive: &oneai_bus::Directive,
    expected_kind: &str,
) -> Value
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let line = oneai_bus::serialize_directive(directive).expect("serialize directive");
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let mut buf = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        buf.clear();
        let n = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            reader.read_line(&mut buf),
        )
        .await
        .expect("timed out reading yield")
        .expect("read_line");
        assert!(n > 0, "sidecar closed the connection");
        let val: Value = serde_json::from_str(buf.trim()).expect("yield is JSON");
        if val["kind"] == expected_kind {
            return val;
        }
        // Tolerate intermediate yields (e.g. an Error if a prior state leaked).
    }
}

/// Poll until the UDS socket accepts a connection (sidecar is ready), with a
/// generous timeout for the cold-start build of the subprocess's first turn.
fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if StdUnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("sidecar socket never came up at {}", path.display());
}

#[tokio::test]
async fn sidecar_roundtrips_provider_free_directives() {
    let bin = env!("CARGO_BIN_EXE_oneai");
    let sock = std::env::temp_dir().join(format!("oneai-serve-e2e-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    let mut child = std::process::Command::new(bin)
        .arg("serve")
        .arg("--socket")
        .arg(&sock)
        .arg("--domain")
        .arg("coding")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oneai serve");

    // If anything below fails, surface the sidecar's stderr for debugging.
    let mut stderr = child.stderr.take().expect("piped stderr");
    let result = run_assertions(&sock, &mut stderr).await;
    if result.is_err() {
        // Drain stderr into the panic message.
        use std::io::Read;
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        let _ = result
            .map_err(|e| panic!("sidecar assertion failed: {e}\n--- sidecar stderr ---\n{buf}"));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&sock);
}

async fn run_assertions(
    sock: &std::path::Path,
    _stderr: &mut std::process::ChildStderr,
) -> Result<(), String> {
    wait_for_socket(sock);

    let stream = tokio::net::UnixStream::connect(sock)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (read, write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut writer = write;

    // 1. ClearSession → SessionCleared (no provider needed; just swaps the
    //    AppSession). Proves directive → pump → SidecarRuntime → yield → wire.
    let cleared = roundtrip(
        &mut reader,
        &mut writer,
        &oneai_bus::Directive::ClearSession,
        "session_cleared",
    )
    .await;
    assert!(
        cleared["id"].as_str().is_some(),
        "SessionCleared carries id"
    );

    // 2. SwitchParadigm → ParadigmSwitch (also provider-free; sync set_paradigm).
    let switched = roundtrip(
        &mut reader,
        &mut writer,
        &oneai_bus::Directive::SwitchParadigm {
            to: oneai_bus::BusParadigmKind::Plan,
        },
        "paradigm_switch",
    )
    .await;
    assert_eq!(switched["to"], "plan", "switched to Plan: {switched}");

    Ok(())
}
