//! W3C Trace Context (`traceparent`) support — gap-analysis P0 #4.
//!
//! Distributed propagation of OneAI traces across sub-agents and A2A calls.
//! OneAI spans use UUID v4 ids; W3C `traceparent` uses a 128-bit trace-id +
//! 64-bit parent-id. The mapping mirrors the OTEL exporter
//! ([`crate::otel_exporter`]): strip the UUID dashes to 32 hex chars, use
//! the span-tree ROOT's id as the trace-id, and truncate/pad a span id to
//! 16 hex chars for the parent-id — so a remote system that honours W3C
//! Trace Context attaches our spans under the same trace.
//!
//! Header format (per <https://www.w3.org/TR/trace-context/>):
//! `00-{trace-id:32hex}-{parent-id:16hex}-{flags:2hex}`.

/// The W3C Trace Context HTTP header name.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// A parsed W3C `traceparent` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traceparent {
    /// Protocol version (currently `00`).
    pub version: u8,
    /// 128-bit trace id — 32 lowercase hex chars.
    pub trace_id: String,
    /// 64-bit parent span id — 16 lowercase hex chars.
    pub parent_id: String,
    /// Trace flags (bit 0 = sampled).
    pub flags: u8,
}

impl Traceparent {
    /// Whether the sampled flag is set.
    pub fn sampled(&self) -> bool {
        self.flags & 0x01 == 0x01
    }

    /// Serialize back to the wire format.
    pub fn to_header(&self) -> String {
        format!(
            "{:02x}-{}-{}-{:02x}",
            self.version, self.trace_id, self.parent_id, self.flags
        )
    }
}

/// Format a `traceparent` header value from a 32-hex trace id and a 16-hex
/// parent id. Inputs are expected to already be shaped (see
/// [`w3c_trace_id`] / [`w3c_span_id`]).
pub fn format_traceparent(trace_id: &str, parent_id: &str, sampled: bool) -> String {
    format!(
        "00-{}-{}-{:02x}",
        trace_id.to_ascii_lowercase(),
        parent_id.to_ascii_lowercase(),
        u8::from(sampled)
    )
}

/// Parse a `traceparent` header value. Returns `None` on any format
/// violation (wrong field count, non-hex chars, wrong lengths, all-zero
/// ids) — callers must treat a malformed header as "no parent".
pub fn parse_traceparent(header: &str) -> Option<Traceparent> {
    let mut parts = header.trim().split('-');
    let version = u8::from_str_radix(parts.next()?, 16).ok()?;
    let trace_id = parts.next()?.to_ascii_lowercase();
    let parent_id = parts.next()?.to_ascii_lowercase();
    let flags = u8::from_str_radix(parts.next()?, 16).ok()?;
    // Extra trailing fields are allowed by the spec for future versions,
    // but version 00 defines exactly four — accept them silently either way.
    if trace_id.len() != 32 || parent_id.len() != 16 {
        return None;
    }
    if !trace_id.chars().all(|c| c.is_ascii_hexdigit())
        || !parent_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    // All-zero ids are invalid per spec.
    if trace_id.chars().all(|c| c == '0') || parent_id.chars().all(|c| c == '0') {
        return None;
    }
    Some(Traceparent {
        version,
        trace_id,
        parent_id,
        flags,
    })
}

/// Strip a OneAI UUID span id (e.g. `550e8400-e29b-...`) to its 32 hex chars.
pub fn uuid_to_hex(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_hexdigit()).collect()
}

/// 16-hex (8-byte) W3C/OTEL span id derived from a OneAI span id
/// (truncate a UUID's 32 hex chars to the first 16; pad short inputs).
pub fn w3c_span_id(id: &str) -> String {
    let mut out = uuid_to_hex(id);
    if out.len() > 16 {
        out.truncate(16);
    }
    while out.len() < 16 {
        out.insert(0, '0');
    }
    out
}

/// Pad/trim a hex string to exactly 32 chars (16-byte W3C/OTEL trace id).
pub fn w3c_trace_id(hex: &str) -> String {
    let mut out = hex.to_string();
    if out.len() > 32 {
        out.truncate(32);
    }
    while out.len() < 32 {
        out.insert(0, '0');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse_roundtrip() {
        let header =
            format_traceparent("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331", true);
        assert_eq!(
            header,
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        );
        let parsed = parse_traceparent(&header).unwrap();
        assert_eq!(parsed.version, 0);
        assert_eq!(parsed.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(parsed.parent_id, "b7ad6b7169203331");
        assert!(parsed.sampled());
        assert_eq!(parsed.to_header(), header);
    }

    #[test]
    fn parse_uppercase_normalizes() {
        let parsed =
            parse_traceparent("00-0AF7651916CD43DD8448EB211C80319C-B7AD6B7169203331-00").unwrap();
        assert_eq!(parsed.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert!(!parsed.sampled());
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_traceparent("").is_none());
        assert!(parse_traceparent("garbage").is_none());
        // wrong lengths
        assert!(parse_traceparent("00-abc-def-01").is_none());
        // non-hex
        assert!(
            parse_traceparent("00-zzf7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").is_none()
        );
        // all-zero trace id
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-b7ad6b7169203331-01").is_none()
        );
        // all-zero parent id
        assert!(
            parse_traceparent("00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01").is_none()
        );
        // bad version field (not hex)
        assert!(
            parse_traceparent("zz-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").is_none()
        );
    }

    #[test]
    fn uuid_mapping_shapes() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(uuid_to_hex(uuid), "550e8400e29b41d4a716446655440000");
        assert_eq!(w3c_span_id(uuid), "550e8400e29b41d4");
        assert_eq!(
            w3c_trace_id(&uuid_to_hex(uuid)),
            "550e8400e29b41d4a716446655440000"
        );
        // short input is left-padded
        assert_eq!(w3c_span_id("abc"), "0000000000000abc");
        assert_eq!(w3c_trace_id("abc"), "00000000000000000000000000000abc");
    }
}
