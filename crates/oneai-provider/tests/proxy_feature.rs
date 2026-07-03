//! Smoke test guarding the reqwest proxy feature flags.
//!
//! OneAI routes every outbound HTTP request (LLM providers, web_search /
//! web_fetch, A2A, embedding, MCP HTTP transport) through `reqwest::Client`.
//! Proxy support is therefore a workspace-level concern: the `socks` and
//! `system-proxy` features must stay enabled on the workspace `reqwest` dep.
//!
//! These tests fail loudly if someone trims those features later — e.g.
//! without `socks`, `Proxy::all("socks5://...")` parses but `Client::build()`
//! rejects the scheme; without `client-proxy` (forced on by reqwest itself)
//! the `Proxy` type wouldn't even compile.

#[cfg(test)]
mod tests {
    /// HTTP/HTTPS proxy URL parsing must succeed — these schemes are
    /// supported unconditionally by reqwest.
    #[test]
    fn http_https_proxy_parses() {
        let _ = reqwest::Proxy::http("http://127.0.0.1:7890").expect("http proxy");
        let _ = reqwest::Proxy::https("http://127.0.0.1:7890").expect("https proxy");
    }

    /// A client built with an explicit SOCKS5 proxy must succeed — this
    /// only compiles *and* builds when the `socks` feature is enabled.
    /// Without it, `build()` returns an error mentioning the `socks` feature.
    #[test]
    fn socks5_proxy_client_builds() {
        let proxy = reqwest::Proxy::all("socks5://127.0.0.1:1080")
            .expect("socks5 proxy URL should parse");
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .build()
            .expect("client with socks5 proxy must build (reqwest `socks` feature on)");
        // Drop to silence unused_binding warnings; the build succeeding is the assertion.
        drop(client);
    }

    /// Sanity: a client with an explicit HTTP proxy builds too.
    #[test]
    fn http_proxy_client_builds() {
        let proxy = reqwest::Proxy::http("http://127.0.0.1:7890").unwrap();
        let _client = reqwest::Client::builder()
            .proxy(proxy)
            .build()
            .expect("client with http proxy must build");
    }
}
