//! Local HTTP CONNECT proxy — the sandboxed-process egress gate
//! (#28 Stage 1).
//!
//! The sandbox restricts a `code_interpreter` script to loopback-only network,
//! so the script's outbound HTTPS calls (via `HTTPS_PROXY=http://127.0.0.1:PORT`)
//! are funnelled here. The proxy:
//!
//! 1. Reads the request line — only `CONNECT host:port HTTP/1.1` is honoured
//!    (HTTPS is pip/git/npm's path; plain-HTTP proxying is out of scope for v1).
//! 2. Checks the [`HostAllowlistStore`] — an already-approved host is tunneled
//!    straight through.
//! 3. For an un-approved host, asks the [`InteractionGate`] via
//!    [`InteractionRequest::NetworkApproval`]. `Proceed` records the host in
//!    the session allow-list and tunnels; `Abort` closes the connection (the
//!    script sees a connection error).
//!
//! The proxy is plain byte-relay after the CONNECT handshake — it never
//! terminates TLS, so end-to-end encryption to the real upstream is preserved.
//! Zero new dependencies (tokio only).

use std::sync::Arc;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::InteractionGate;
use oneai_core::{InteractionRequest, InteractionResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::host_allowlist::HostAllowlistStore;

/// A local CONNECT-tunneling egress proxy.
pub struct NetworkProxy {
    listener: TcpListener,
    allowlist: Arc<dyn HostAllowlistStore>,
    gate: Arc<dyn InteractionGate>,
    /// Attribution string for the `NetworkApproval` request (which tool raised
    /// the egress attempt).
    requested_by: String,
}

impl NetworkProxy {
    /// Bind to `127.0.0.1:0` and return the proxy + the assigned port.
    pub async fn bind(
        gate: Arc<dyn InteractionGate>,
        allowlist: Arc<dyn HostAllowlistStore>,
        requested_by: impl Into<String>,
    ) -> Result<(Self, u16)> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| OneAIError::Other(format!("network proxy bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| OneAIError::Other(format!("network proxy local_addr failed: {e}")))?
            .port();
        Ok((
            Self {
                listener,
                allowlist,
                gate,
                requested_by: requested_by.into(),
            },
            port,
        ))
    }

    /// The bound port (for callers that built the listener themselves / tests).
    pub fn local_port(&self) -> Result<u16> {
        self.listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| OneAIError::Other(format!("network proxy local_addr: {e}")))
    }

    /// Accept loop. Runs until the listener errors. Each connection is handled
    /// in its own task so a slow tunnel never blocks the next CONNECT.
    pub async fn run(self) {
        let Self {
            listener,
            allowlist,
            gate,
            requested_by,
        } = self;
        loop {
            match listener.accept().await {
                Ok((conn, _peer)) => {
                    let allowlist = allowlist.clone();
                    let gate = gate.clone();
                    let requested_by = requested_by.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(conn, allowlist, gate, &requested_by).await {
                            tracing::debug!("network proxy connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("network proxy accept failed: {e}; exiting run loop");
                    return;
                }
            }
        }
    }
}

/// Handle a single inbound CONNECT request.
async fn handle_conn(
    client: TcpStream,
    allowlist: Arc<dyn HostAllowlistStore>,
    gate: Arc<dyn InteractionGate>,
    requested_by: &str,
) -> Result<()> {
    // Own the stream in a BufReader so we can read line-delimited request
    // data; recover it with `into_inner` once the handshake is parsed.
    let mut reader = BufReader::new(client);
    let mut line = Vec::with_capacity(512);
    reader
        .read_until(b'\n', &mut line)
        .await
        .map_err(|e| OneAIError::Other(format!("network proxy: read request line: {e}")))?;
    if line.is_empty() {
        return Ok(()); // client hung up
    }
    // Drain the rest of the request headers up to the blank line so they don't
    // corrupt the tunneled stream.
    drain_headers(&mut reader).await;

    let line_str = String::from_utf8_lossy(&line);
    let request_line = line_str.trim_end_matches(['\r', '\n']);

    // Parse `CONNECT host:port HTTP/1.1`.
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let host_port = parts.next().unwrap_or("");

    let mut client = reader.into_inner();

    if method != "CONNECT" || host_port.is_empty() {
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Ok(());
    }

    let host = host_port
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Ok(());
    }

    // Allow-list short-circuit; else ask the gate.
    let admitted = if allowlist.is_allowed(&host).await {
        true
    } else {
        let resp = gate
            .request(InteractionRequest::NetworkApproval {
                host: host.clone(),
                requested_by: requested_by.to_string(),
            })
            .await?;
        match resp {
            InteractionResponse::Proceed | InteractionResponse::ProceedWith { .. } => {
                allowlist.add(host.clone()).await;
                tracing::info!("network proxy: host '{}' admitted by user", host);
                true
            }
            InteractionResponse::Abort { ref reason } => {
                tracing::info!("network proxy: host '{}' denied: {}", host, reason);
                false
            }
            // Revise/Choose don't apply to NetworkApproval; treat as deny.
            _ => false,
        }
    };

    if !admitted {
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Ok(());
    }

    // Connect to the real upstream and tunnel. The client waits for the 200
    // before sending TLS, so no pre-200 bytes are pending.
    let mut upstream = match TcpStream::connect(host_port).await {
        Ok(s) => s,
        Err(e) => {
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            tracing::warn!(
                "network proxy: upstream connect {} failed: {}",
                host_port,
                e
            );
            return Ok(());
        }
    };

    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .map_err(|e| OneAIError::Other(format!("network proxy: write 200: {e}")))?;

    // Plain byte relay — no TLS termination; end-to-end encryption preserved.
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// Drain HTTP request headers (everything until a blank line) so they don't
/// bleed into the tunnel.
async fn drain_headers(reader: &mut BufReader<TcpStream>) {
    let mut buf = Vec::with_capacity(256);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let trimmed = buf.iter().filter(|&&b| b != b'\r' && b != b'\n').count();
                if trimmed == 0 {
                    return; // blank line — end of headers
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_allowlist::InMemoryHostAllowlist;
    use oneai_core::traits::InteractionGate;
    use oneai_core::InteractionPoint;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    /// A gate that approves everything (mirrors Noop but returns Proceed for
    /// NetworkApproval regardless of `enabled`).
    struct ApproveAll;
    #[async_trait::async_trait]
    impl InteractionGate for ApproveAll {
        async fn request(&self, _req: InteractionRequest) -> Result<InteractionResponse> {
            Ok(InteractionResponse::Proceed)
        }
        fn enabled(&self, _point: InteractionPoint) -> bool {
            true
        }
    }

    /// A gate that denies everything.
    struct DenyAll;
    #[async_trait::async_trait]
    impl InteractionGate for DenyAll {
        async fn request(&self, _req: InteractionRequest) -> Result<InteractionResponse> {
            Ok(InteractionResponse::Abort {
                reason: "denied".into(),
            })
        }
        fn enabled(&self, _point: InteractionPoint) -> bool {
            true
        }
    }

    /// Spin up a local echo TCP server the proxy can tunnel to.
    async fn echo_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let host = format!("127.0.0.1:{}", addr.port());
        let h = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 16];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let _ = sock.write_all(&buf[..n]).await;
            }
        });
        (host, h)
    }

    #[tokio::test]
    async fn approved_host_tunnels_and_echoes() {
        let allowlist = Arc::new(InMemoryHostAllowlist::new());
        allowlist.add("127.0.0.1".to_string()).await; // pre-approve loopback host
        let (proxy, port) = NetworkProxy::bind(Arc::new(ApproveAll), allowlist.clone(), "test")
            .await
            .unwrap();
        tokio::spawn(proxy.run());

        let (upstream_host, _echo) = echo_server().await;

        // Client → proxy: CONNECT upstream_host
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        client
            .write_all(format!("CONNECT {upstream_host} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let n = client.read(&mut buf).await.unwrap();
        let handshake = String::from_utf8_lossy(&buf[..n]);
        assert!(handshake.contains("200 Connection established"));
        client.write_all(b"hello").await.unwrap();
        let mut echo = [0u8; 8];
        let n = client.read(&mut echo).await.unwrap();
        assert_eq!(&echo[..n], b"hello");
    }

    #[tokio::test]
    async fn unapproved_host_under_approve_gate_is_admitted_and_recorded() {
        let allowlist = Arc::new(InMemoryHostAllowlist::new());
        let (proxy, port) = NetworkProxy::bind(Arc::new(ApproveAll), allowlist.clone(), "test")
            .await
            .unwrap();
        tokio::spawn(proxy.run());

        let (upstream_host, _echo) = echo_server().await;
        // host is the bare 127.0.0.1 (loopback echo) — not pre-approved, gate
        // approves → recorded.
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        client
            .write_all(format!("CONNECT {upstream_host} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let _ = client.read(&mut buf).await.unwrap();
        assert!(allowlist.is_allowed("127.0.0.1").await);
    }

    #[tokio::test]
    async fn deny_gate_returns_403() {
        let allowlist = Arc::new(InMemoryHostAllowlist::new());
        let (proxy, port) = NetworkProxy::bind(Arc::new(DenyAll), allowlist.clone(), "test")
            .await
            .unwrap();
        tokio::spawn(proxy.run());

        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        client
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let n = client.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("403 Forbidden"));
        assert!(!allowlist.is_allowed("example.com").await);
    }

    #[tokio::test]
    async fn non_connect_method_rejected() {
        let allowlist = Arc::new(InMemoryHostAllowlist::new());
        let (proxy, port) = NetworkProxy::bind(Arc::new(ApproveAll), allowlist.clone(), "test")
            .await
            .unwrap();
        tokio::spawn(proxy.run());

        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        client
            .write_all(b"GET http://example.com/ HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let n = client.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("403 Forbidden"));
    }
}
