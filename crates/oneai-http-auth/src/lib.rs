//! Shared Bearer-token authentication primitives for OneAI HTTP services.
//!
//! Extracted from the byte-identical copies that lived in
//! `oneai-a2a/src/server.rs` and `oneai-scheduler/src/oneshot.rs` (MVS2 of the
//! cloud orchestrator design, `docs/cloud-orchestrator-design.md` §4/§10).
//! Three consumers converge here:
//!
//! - `oneai-a2a`   — `ONEAI_A2A_SECRET`
//! - `oneai-scheduler` — `ONEAI_CRON_SECRET`
//! - `oneai-orchestrator` — `ONEAI_ORCHESTRATOR_SECRET`
//!
//! The env-var *name* stays per-crate (each service has its own secret); this
//! crate provides the mechanics: constant-time comparison, env loading with
//! empty-string filtering, and `Authorization: Bearer <token>` header
//! verification against `axum::http::HeaderMap`.
//!
//! # Example
//!
//! ```
//! use oneai_http_auth::BearerSecret;
//!
//! let secret = BearerSecret::new("s3cret");
//! // In an axum handler:
//! // if let Err(resp) = secret.guard(&headers) { return resp; }
//! assert!(secret.verify_str("s3cret"));
//! ```

use std::sync::Arc;

use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use axum::response::IntoResponse;

/// Constant-time byte-slice equality. `true` iff equal length AND equal
/// content; a mismatch never short-circuits, so bearer comparison doesn't
/// leak length/timing.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Read a bearer secret from the environment variable `var`.
/// Returns `None` if the variable is unset or empty — callers decide the
/// failure mode (refuse to start, 503, …).
pub fn secret_from_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// Verify an `Authorization: Bearer <token>` header against `expected` using
/// constant-time comparison. `false` on a missing header, a non-UTF-8 value,
/// a non-Bearer scheme, or a token mismatch.
pub fn verify_bearer(headers: &HeaderMap, expected: &str) -> bool {
    let Ok(Some(value)) = headers.get(AUTHORIZATION).map(|v| v.to_str()).transpose() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(token.as_bytes(), expected.as_bytes())
}

/// A cheaply-cloneable shared bearer secret for axum services.
///
/// Wraps the secret string in an `Arc` so router state can hand it to every
/// handler. [`BearerSecret::guard`] is the one-liner for handler bodies:
/// it returns an axum 401 `Response` on verification failure.
#[derive(Clone, Debug)]
pub struct BearerSecret {
    secret: Arc<String>,
}

impl BearerSecret {
    /// Wrap a literal/config-provided secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: Arc::new(secret.into()),
        }
    }

    /// Load from the environment variable `var`. `None` if unset or empty
    /// (callers typically refuse to start — mirrors the a2a/scheduler
    /// "external triggering disabled until a secret is set" semantics).
    pub fn from_env(var: &str) -> Option<Self> {
        secret_from_env(var).map(Self::new)
    }

    /// The secret value (for injecting into child processes / containers).
    pub fn expose(&self) -> &str {
        &self.secret
    }

    /// Verify request headers against this secret.
    pub fn verify(&self, headers: &HeaderMap) -> bool {
        verify_bearer(headers, &self.secret)
    }

    /// Verify a raw token string (non-HTTP contexts, tests).
    pub fn verify_str(&self, token: &str) -> bool {
        ct_eq(token.as_bytes(), self.secret.as_bytes())
    }

    /// Handler-body guard: `None` when the request carries the right bearer
    /// token, `Some(response)` with a ready-to-return 401 otherwise.
    /// (`Option` rather than `Result` — axum's `Response` is large and
    /// clippy's `result_large_err` rightly objects to it in the Err slot.)
    pub fn guard(&self, headers: &HeaderMap) -> Option<axum::response::Response> {
        if self.verify(headers) {
            None
        } else {
            Some(
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                    "unauthorized",
                )
                    .into_response(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn verify_bearer_header() {
        assert!(verify_bearer(&headers_with("Bearer tok"), "tok"));
        assert!(!verify_bearer(&headers_with("Bearer tok"), "other"));
        assert!(!verify_bearer(&headers_with("Basic tok"), "tok"));
        assert!(!verify_bearer(&headers_with("tok"), "tok"));
        assert!(!verify_bearer(&HeaderMap::new(), "tok"));
    }

    #[test]
    fn verify_bearer_non_utf8_rejected() {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff\xfe").unwrap(),
        );
        assert!(!verify_bearer(&h, "tok"));
    }

    #[test]
    fn secret_from_env_filters_empty() {
        // Use a var name unlikely to collide; set + remove within the test.
        let var = "ONEAI_HTTP_AUTH_TEST_SECRET";
        std::env::set_var(var, "");
        assert_eq!(secret_from_env(var), None);
        std::env::set_var(var, "v");
        assert_eq!(secret_from_env(var).as_deref(), Some("v"));
        std::env::remove_var(var);
        assert_eq!(secret_from_env(var), None);
    }

    #[test]
    fn bearer_secret_guard_and_verify() {
        let s = BearerSecret::new("topsecret");
        assert!(s.guard(&headers_with("Bearer topsecret")).is_none());
        let resp = s.guard(&headers_with("Bearer wrong")).unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert!(s.verify_str("topsecret"));
        assert!(!s.verify_str("nope"));
        assert_eq!(s.expose(), "topsecret");
    }
}
