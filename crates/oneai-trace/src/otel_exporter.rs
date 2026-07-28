//! OTEL exporter — bridges OneAI TraceCollector to OpenTelemetry OTLP protocol.
//!
//! When the `otel` feature is enabled, this module provides:
//! - `OtlpCollector`: a `TraceCollector` implementation that converts OneAI spans
//!   to OTEL spans and **really exports them via OTLP/HTTP** to a collector
//!   (Jaeger, Tempo, OTEL-Collector, …) — no longer just buffering locally.
//! - `OtlpExporter` trait + `HttpOtlpExporter` (reqwest, OTLP/JSON POST to
//!   `/v1/traces`) + `InMemoryOtlpExporter` (test double). The exporter is
//!   injectable so the export path is unit-testable without a live collector.
//! - `OtlpConfig`: configuration for the OTEL exporter (endpoint, protocol,
//!   service name). `OtlpMetricsProvider`: OTEL metrics for agent observability.
//!
//! ## Real export vs. the old local-only stub
//!
//! Previously `on_span_end` pushed spans into a local `Vec` and `flush()` was a
//! no-op — the OTLP endpoint never received anything (the gap-analysis #4
//! "虚假安全感" stub). Now `flush()` drains the buffer through the configured
//! `OtlpExporter`; `HttpOtlpExporter` POSTs an OTLP/JSON `resourceSpans` payload
//! to `{endpoint}/v1/traces`. The local buffer is retained only for
//! `export_json()` debugging.
//!
//! ## Architecture
//!
//! The OneAI → OTEL bridge maps:
//! - `SpanKind` → OTEL span kind (CLIENT, SERVER, INTERNAL, etc.)
//! - `SpanStatus::Ok` → OTEL Status::Ok
//! - `SpanStatus::Error` → OTEL Status::Error with description
//! - `SpanStatus::Cancelled` → OTEL Status::Error with "cancelled" description
//! - `EventKind` → OTEL span events with semantic attribute conventions
//! - `TraceEvent.attributes` → OTEL span event attributes
//!
//! ## Usage
//!
//! ```ignore
//! let config = OtlpConfig::http("http://localhost:4318", "oneai-agent");
//! let collector = OtlpCollector::new(config);
//! let ctx = TraceEmitter::global().create_context_with_collector(Arc::new(collector));
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::collector::TraceCollector;
use crate::event::{EventKind, TraceEvent};
use crate::span::{Span, SpanKind, SpanStatus};

// ─── OtlpConfig ──────────────────────────────────────────────────────

/// Configuration for the OTEL OTLP exporter.
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// OTLP endpoint URL (e.g., "http://localhost:4317" for gRPC, "http://localhost:4318" for HTTP).
    pub endpoint: String,

    /// Export protocol: gRPC or HTTP.
    pub protocol: OtlpProtocol,

    /// Service name for OTEL resource attribution.
    pub service_name: String,

    /// Additional resource attributes (e.g., version, deployment environment).
    pub resource_attributes: HashMap<String, String>,
}

/// OTLP export protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    /// gRPC protocol (default, more efficient).
    Grpc,
    /// HTTP/protobuf protocol (easier to set up, works with proxies).
    Http,
}

impl OtlpConfig {
    /// Create a gRPC config with endpoint and service name.
    pub fn grpc(endpoint: &str, service_name: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            protocol: OtlpProtocol::Grpc,
            service_name: service_name.to_string(),
            resource_attributes: HashMap::new(),
        }
    }

    /// Create an HTTP config with endpoint and service name.
    pub fn http(endpoint: &str, service_name: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            protocol: OtlpProtocol::Http,
            service_name: service_name.to_string(),
            resource_attributes: HashMap::new(),
        }
    }

    /// Add a resource attribute.
    pub fn with_attribute(mut self, key: &str, value: &str) -> Self {
        self.resource_attributes
            .insert(key.to_string(), value.to_string());
        self
    }
}

// ─── OtlpExporter trait + implementations ───────────────────────────

/// A batch of completed OneAI spans ready to be exported as one OTLP request.
///
/// Carries the OTEL resource attributes (service name + user attributes) so the
/// exporter can attach them to the `resourceSpans` block of the OTLP payload.
pub struct ExportBatch {
    /// OTEL service.name (resource attribute).
    pub service_name: String,
    /// Additional resource attributes from `OtlpConfig::resource_attributes`.
    pub resource_attributes: HashMap<String, String>,
    /// Completed OneAI spans to export in this batch.
    pub spans: Vec<Span>,
}

/// Exporter abstraction — turns a batch of OneAI spans into a real OTLP export.
///
/// Implementations:
/// - [`HttpOtlpExporter`]: real OTLP/HTTP (OTLP/JSON) POST via `reqwest`,
///   honoring OneAI's proxy env vars (`HTTPS_PROXY`/`NO_PROXY`/…).
/// - [`InMemoryOtlpExporter`]: test double that captures every batch in-memory
///   so the export path can be asserted without a live collector.
#[async_trait]
pub trait OtlpExporter: Send + Sync {
    /// Export one batch. Returns `Err(message)` on transport failure; the
    /// caller (`OtlpCollector::flush`) surfaces it but never panics.
    async fn export(&self, batch: ExportBatch) -> Result<(), String>;
}

/// Real OTLP/HTTP exporter — POSTs an OTLP/JSON `resourceSpans` payload to
/// `{endpoint}/v1/traces` using `reqwest`.
///
/// OTLP/HTTP is a standard OTEL protocol
/// (https://opentelemetry.io/docs/specs/otlp/#otlphttp); a standard collector
/// (Jaeger, Tempo, OTEL-Collector) accepts these requests on port 4318. The
/// `reqwest::Client` is built fresh so it picks up OneAI's proxy env vars per
/// the convention in `CLAUDE.md` (all outbound HTTP via reqwest).
///
/// **Note on gRPC**: true OTLP/gRPC (protobuf over HTTP/2) is not implemented
/// here — it would require the tonic stack and can't be exercised in CI
/// without a live collector. When `OtlpConfig::protocol == Grpc`, this
/// exporter still POSTs OTLP/HTTP to the configured endpoint and logs a
/// `tracing::warn` recommending the collector's HTTP port instead.
pub struct HttpOtlpExporter {
    client: reqwest::Client,
    /// Full traces URL (`{base}/v1/traces`).
    traces_url: String,
}

impl HttpOtlpExporter {
    /// Construct from an `OtlpConfig`. Builds the OTLP/HTTP traces URL and a
    /// `reqwest::Client` (infallible under the workspace's `rustls-tls` feature).
    pub fn new(config: &OtlpConfig) -> Self {
        let traces_url = normalize_traces_url(&config.endpoint);
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest Client build (rustls-tls) cannot fail at runtime");
        if config.protocol == OtlpProtocol::Grpc {
            tracing::warn!(
                "OTLP/gRPC export is not implemented; posting OTLP/HTTP to {}. \
                 Point the endpoint at the collector's OTLP/HTTP port (e.g. http://localhost:4318) \
                 for real delivery.",
                traces_url
            );
        }
        Self { client, traces_url }
    }
}

#[async_trait]
impl OtlpExporter for HttpOtlpExporter {
    async fn export(&self, batch: ExportBatch) -> Result<(), String> {
        if batch.spans.is_empty() {
            return Ok(());
        }
        let payload = build_otlp_json_payload(&batch);
        let resp = self
            .client
            .post(&self.traces_url)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("OTLP HTTP export failed: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(format!("OTLP collector returned {status}: {body}"))
        }
    }
}

/// Test double — captures every exported batch in memory so the export path
/// can be asserted without a live OTEL collector.
pub struct InMemoryOtlpExporter {
    batches: std::sync::Mutex<Vec<ExportBatch>>,
}

impl InMemoryOtlpExporter {
    /// Create an empty capturing exporter.
    pub fn new() -> Self {
        Self {
            batches: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Number of batches exported so far.
    pub fn batch_count(&self) -> usize {
        self.batches.lock().unwrap().len()
    }

    /// Total spans across all exported batches.
    pub fn total_spans(&self) -> usize {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .map(|b| b.spans.len())
            .sum()
    }

    /// All exported spans, flattened (newest batch last).
    pub fn exported_spans(&self) -> Vec<Span> {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| b.spans.clone())
            .collect()
    }
}

impl Default for InMemoryOtlpExporter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OtlpExporter for InMemoryOtlpExporter {
    async fn export(&self, batch: ExportBatch) -> Result<(), String> {
        self.batches.lock().unwrap().push(batch);
        Ok(())
    }
}

// ─── OTLP/JSON payload construction ─────────────────────────────────

/// Normalize an endpoint into a full OTLP/HTTP traces URL.
///
/// `http://localhost:4318` → `http://localhost:4318/v1/traces`.
/// `http://localhost:4318/v1/traces` → unchanged. Trims trailing slashes.
fn normalize_traces_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/traces")
    }
}

/// OTEL SpanKind numeric code per the OTLP spec.
///
/// 0=UNSPECIFIED, 1=INTERNAL, 2=SERVER, 3=CLIENT, 4=PRODUCER, 5=CONSUMER.
fn span_kind_to_otel_code(kind: &SpanKind) -> u8 {
    match span_kind_to_otel(kind) {
        "INTERNAL" => 1,
        "SERVER" => 2,
        "CLIENT" => 3,
        _ => 0,
    }
}

/// OTEL Status code: 1=Ok, 2=Error (0=Unset unused).
fn span_status_to_otel_code(status: &SpanStatus) -> u8 {
    match status {
        SpanStatus::Ok => 1,
        SpanStatus::Error | SpanStatus::Cancelled => 2,
    }
}

/// Strip a OneAI UUID span id (e.g. `550e8400-e29b-...`) to its 32 hex chars.
fn hex_span_id(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_hexdigit()).collect()
}

/// 16-hex (8-byte) OTEL span id derived from a OneAI span id.
fn otel_span_id(id: &str) -> String {
    let hex = hex_span_id(id);
    // OTLP spanId is exactly 16 hex chars (8 bytes). Pad/trim to 16.
    let mut out = hex;
    if out.len() > 16 {
        out.truncate(16);
    }
    while out.len() < 16 {
        out.insert(0, '0');
    }
    out
}

/// 32-hex (16-byte) OTEL trace id. Resolved by walking the parent chain to
/// the root span within the batch and using the root's span id as the trace
/// id (so all spans in one tree share a trace id). Falls back to the span's
/// own id (or its parent's) if the root isn't present in this batch.
fn otel_trace_id(span: &Span, by_id: &HashMap<String, &Span>) -> String {
    let mut current = span;
    let mut guard = 0;
    while let Some(parent_id) = current.parent_span_id.as_deref() {
        match by_id.get(parent_id) {
            Some(parent) => {
                current = parent;
                guard += 1;
                if guard > 64 {
                    break; // cycle guard
                }
            }
            None => {
                // Parent not in this batch — use the parent id as the trace id.
                return pad_trace_id(&hex_span_id(parent_id));
            }
        }
    }
    pad_trace_id(&hex_span_id(&current.span_id))
}

/// Pad/trim a hex string to exactly 32 chars (16-byte OTEL trace id).
fn pad_trace_id(hex: &str) -> String {
    let mut out = hex.to_string();
    if out.len() > 32 {
        out.truncate(32);
    }
    while out.len() < 32 {
        out.insert(0, '0');
    }
    out
}

/// Convert a `serde_json::Value` to an OTLP `anyValue` JSON object.
fn json_to_otlp_anyvalue(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::json!({"stringValue": s}),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::json!({"intValue": i.to_string()})
            } else if let Some(u) = n.as_u64() {
                serde_json::json!({"intValue": u.to_string()})
            } else if let Some(f) = n.as_f64() {
                serde_json::json!({"doubleValue": f})
            } else {
                serde_json::json!({"stringValue": v.to_string()})
            }
        }
        serde_json::Value::Bool(b) => serde_json::json!({"boolValue": b}),
        serde_json::Value::Null => serde_json::json!({"stringValue": ""}),
        // Arrays/objects: stringify (OTLP anyValue supports arrays but this is enough).
        other => serde_json::json!({"stringValue": other.to_string()}),
    }
}

/// Convert OneAI span attributes (dot-namespaced → OneAI-namespaced for OTEL)
/// to OTLP attribute objects.
fn attributes_to_otlp(span: &Span) -> Vec<serde_json::Value> {
    span.attributes
        .iter()
        .map(|(k, v)| {
            serde_json::json!({
                "key": format!("oneai.{k}"),
                "value": json_to_otlp_anyvalue(v),
            })
        })
        .collect()
}

/// Convert OneAI events to OTLP span events (`timeUnixNano` + name + attrs).
fn events_to_otlp(span: &Span) -> Vec<serde_json::Value> {
    span.events
        .iter()
        .map(|event| {
            let name = event_kind_to_otel_name(&event.kind);
            let attrs: Vec<serde_json::Value> = event
                .attributes
                .iter()
                .map(|(k, v)| {
                    serde_json::json!({
                        "key": format!("oneai.{k}"),
                        "value": json_to_otlp_anyvalue(v),
                    })
                })
                .collect();
            serde_json::json!({
                "name": name,
                "timeUnixNano": event.timestamp.timestamp_nanos_opt().unwrap_or(0).max(0).to_string(),
                "attributes": attrs,
            })
        })
        .collect()
}

/// Build the OTLP/JSON `resourceSpans` payload for one batch.
fn build_otlp_json_payload(batch: &ExportBatch) -> serde_json::Value {
    // Index spans by id for trace-id root resolution.
    let by_id: HashMap<String, &Span> =
        batch.spans.iter().map(|s| (s.span_id.clone(), s)).collect();

    let mut resource_attrs: Vec<serde_json::Value> = vec![serde_json::json!({
        "key": "service.name",
        "value": {"stringValue": batch.service_name.clone()},
    })];
    for (k, v) in &batch.resource_attributes {
        resource_attrs.push(serde_json::json!({
            "key": k,
            "value": {"stringValue": v},
        }));
    }

    let spans: Vec<serde_json::Value> = batch
        .spans
        .iter()
        .map(|span| {
            let trace_id = otel_trace_id(span, &by_id);
            let span_id = otel_span_id(&span.span_id);
            let parent_span_id = span.parent_span_id.as_ref().map(|p| otel_span_id(p));
            let start_nanos = span
                .start_time
                .timestamp_nanos_opt()
                .unwrap_or(0)
                .max(0)
                .to_string();
            let end_nanos = span
                .end_time
                .and_then(|t| t.timestamp_nanos_opt())
                .unwrap_or(0)
                .max(0)
                .to_string();
            let code = span_status_to_otel_code(&span.status);
            let mut status = serde_json::json!({"code": code});
            if code == 2 {
                if let (_, Some(msg)) = span_status_to_otel(&span.status) {
                    status["message"] = serde_json::json!(msg);
                }
            }
            let mut span_obj = serde_json::json!({
                "traceId": trace_id,
                "spanId": span_id,
                "name": span.name,
                "kind": span_kind_to_otel_code(&span.kind),
                "startTimeUnixNano": start_nanos,
                "endTimeUnixNano": end_nanos,
                "status": status,
                "attributes": attributes_to_otlp(span),
                "events": events_to_otlp(span),
            });
            if let Some(pid) = parent_span_id {
                span_obj["parentSpanId"] = serde_json::json!(pid);
            }
            span_obj
        })
        .collect();

    serde_json::json!({
        "resourceSpans": [{
            "resource": {"attributes": resource_attrs},
            "scopeSpans": [{
                "scope": {"name": "oneai-agent"},
                "spans": spans,
            }]
        }]
    })
}

// ─── SpanKind → OTEL mapping ────────────────────────────────────────

/// Convert OneAI SpanKind to OTEL span kind string.
///
/// Maps the OneAI semantic conventions to OpenTelemetry conventions:
/// - SESSION → Internal (root session lifecycle)
/// - AGENT → Internal (agent paradigm execution)
/// - TOOL → Client (tool call = outbound request)
/// - LLM → Client (LLM inference = outbound API call)
/// - RETRIEVER → Client (memory/RAG retrieval)
/// - WORKFLOW → Internal (workflow execution)
/// - APPROVAL → Server (approval gate = waiting for human input)
/// - PARSER → Internal (output parsing)
/// - INTERNAL → Internal
pub fn span_kind_to_otel(kind: &SpanKind) -> &'static str {
    match kind {
        SpanKind::SESSION => "INTERNAL",
        SpanKind::AGENT => "INTERNAL",
        SpanKind::TOOL => "CLIENT",
        SpanKind::LLM => "CLIENT",
        SpanKind::RETRIEVER => "CLIENT",
        SpanKind::WORKFLOW => "INTERNAL",
        SpanKind::APPROVAL => "SERVER",
        SpanKind::PARSER => "INTERNAL",
        SpanKind::INTERNAL => "INTERNAL",
    }
}

/// Convert OneAI SpanStatus to OTEL status.
pub fn span_status_to_otel(status: &SpanStatus) -> (&'static str, Option<&'static str>) {
    match status {
        SpanStatus::Ok => ("Ok", None),
        SpanStatus::Error => ("Error", Some("operation failed")),
        SpanStatus::Cancelled => ("Error", Some("operation cancelled")),
    }
}

/// Convert OneAI EventKind to OTEL event name.
pub fn event_kind_to_otel_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Thought => "agent.thought",
        EventKind::Action => "agent.action",
        EventKind::Observation => "agent.observation",
        EventKind::InferenceStart => "llm.inference.start",
        EventKind::InferenceEnd => "llm.inference.end",
        EventKind::StreamingChunk => "llm.streaming.chunk",
        EventKind::ToolCall => "tool.call",
        EventKind::ToolResult => "tool.result",
        EventKind::ToolError => "tool.error",
        EventKind::ApprovalRequest => "approval.request",
        EventKind::ApprovalResponse => "approval.response",
        EventKind::ParseAttempt => "parser.attempt",
        EventKind::ParseSuccess => "parser.success",
        EventKind::ParseFallback => "parser.fallback",
        EventKind::MemoryStore => "memory.store",
        EventKind::MemoryRetrieve => "memory.retrieve",
        EventKind::WorkflowStepStart => "workflow.step.start",
        EventKind::WorkflowStepEnd => "workflow.step.end",
        EventKind::CheckpointSave => "checkpoint.save",
        EventKind::CheckpointLoad => "checkpoint.load",
        EventKind::Error => "error",
        EventKind::Custom => "custom",
    }
}

// ─── OtlpCollector ──────────────────────────────────────────────────

/// OTEL OTLP collector — bridges OneAI TraceCollector to OpenTelemetry.
///
/// Converts OneAI spans and events to OTEL format and **exports them via OTLP**
/// through the configured [`OtlpExporter`] (real OTLP/HTTP by default). Spans
/// are buffered on `on_span_end` and flushed through the exporter on `flush()`
/// (or eagerly once `batch_size` is reached).
///
/// The collector maintains:
/// - a pending span buffer (started, not yet ended),
/// - a completed span buffer (ended, awaiting flush),
/// - the exporter sink (where spans really go).
///
/// **Thread-safe**: all internal state is protected by Mutex.
pub struct OtlpCollector {
    config: OtlpConfig,
    /// Pending spans (started but not yet ended).
    pending_spans: std::sync::Mutex<HashMap<String, Span>>,
    /// Completed spans (ended, ready for export).
    completed_spans: std::sync::Mutex<Vec<Span>>,
    /// The exporter sink — where completed spans are really sent on flush.
    exporter: Arc<dyn OtlpExporter>,
    /// Flush eagerly once the completed buffer reaches this size.
    batch_size: usize,
}

impl OtlpCollector {
    /// Create a new OTEL collector with the given configuration.
    ///
    /// Constructs a real [`HttpOtlpExporter`] from the config (OTLP/HTTP POST
    /// to `{endpoint}/v1/traces`). For a test-injectable exporter, use
    /// [`OtlpCollector::with_exporter`].
    pub fn new(config: OtlpConfig) -> Self {
        let exporter: Arc<dyn OtlpExporter> = Arc::new(HttpOtlpExporter::new(&config));
        Self {
            config,
            pending_spans: std::sync::Mutex::new(HashMap::new()),
            completed_spans: std::sync::Mutex::new(Vec::new()),
            exporter,
            batch_size: 64,
        }
    }

    /// Create a collector with a custom exporter (e.g. [`InMemoryOtlpExporter`]
    /// in tests, or a gRPC exporter in a downstream crate). The config is still
    /// used for resource attributes / service name on the exported batch.
    pub fn with_exporter(config: OtlpConfig, exporter: Arc<dyn OtlpExporter>) -> Self {
        Self {
            config,
            pending_spans: std::sync::Mutex::new(HashMap::new()),
            completed_spans: std::sync::Mutex::new(Vec::new()),
            exporter,
            batch_size: 64,
        }
    }

    /// Set the eager-flush batch size (default 64). `0` disables eager flush —
    /// spans are only exported on `flush()`.
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    /// Get the configuration.
    pub fn config(&self) -> &OtlpConfig {
        &self.config
    }

    /// Get the count of pending spans (started but not yet ended).
    pub fn pending_count(&self) -> usize {
        self.pending_spans.lock().unwrap().len()
    }

    /// Get the count of completed (buffered, not yet flushed) spans.
    pub fn completed_count(&self) -> usize {
        self.completed_spans.lock().unwrap().len()
    }

    /// Drain all completed spans into one batch and export them via the
    /// configured exporter. Returns the exporter's result.
    ///
    /// This is the real export path — `HttpOtlpExporter` POSTs an OTLP/JSON
    /// payload to the collector endpoint here.
    pub async fn export_batch(&self) -> Result<(), String> {
        let spans = {
            let mut buf = self.completed_spans.lock().unwrap();
            if buf.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *buf)
        };
        let batch = ExportBatch {
            service_name: self.config.service_name.clone(),
            resource_attributes: self.config.resource_attributes.clone(),
            spans,
        };
        self.exporter.export(batch).await
    }

    /// Export all completed spans as OpenInference-compatible JSON.
    ///
    /// This is for debugging — it serializes the local completed buffer (the
    /// same buffer that `export_batch()` drains). Spans already flushed to the
    /// exporter are no longer here.
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        let spans = self.completed_spans.lock().unwrap();
        serde_json::to_string_pretty(&*spans)
    }

    /// Convert a OneAI Span to a simplified OTEL-compatible JSON representation
    /// (debug shape — see [`build_otlp_json_payload`] for the wire OTLP/JSON
    /// payload that is actually POSTed by [`HttpOtlpExporter`]).
    pub fn span_to_otel_json(span: &Span) -> serde_json::Value {
        let (status_code, status_message) = span_status_to_otel(&span.status);

        let otel_events: Vec<serde_json::Value> = span
            .events
            .iter()
            .map(|event| {
                let event_name = event_kind_to_otel_name(&event.kind);
                let attrs: HashMap<String, serde_json::Value> = event
                    .attributes
                    .iter()
                    .map(|(k, v)| (format!("oneai.{}", k), v.clone()))
                    .collect();
                serde_json::json!({
                    "name": event_name,
                    "timestamp": event.timestamp.to_rfc3339(),
                    "attributes": attrs,
                })
            })
            .collect();

        let otel_attributes: HashMap<String, serde_json::Value> = span
            .attributes
            .iter()
            .map(|(k, v)| (format!("oneai.{}", k), v.clone()))
            .collect();

        let children: Vec<serde_json::Value> =
            span.children.iter().map(Self::span_to_otel_json).collect();

        serde_json::json!({
            "traceId": span.parent_span_id.as_deref().or(Some("00000000000000000000000000000000")),
            "spanId": span.span_id,
            "parentSpanId": span.parent_span_id,
            "kind": span_kind_to_otel(&span.kind),
            "name": span.name,
            "startTime": span.start_time.to_rfc3339(),
            "endTime": span.end_time.map(|t| t.to_rfc3339()),
            "durationMs": span.duration_ms,
            "status": {
                "code": status_code,
                "message": status_message,
            },
            "attributes": otel_attributes,
            "events": otel_events,
            "children": children,
        })
    }
}

#[async_trait]
impl TraceCollector for OtlpCollector {
    async fn on_span_start(&self, span: &Span) {
        // Store the span as pending (started but not yet ended)
        self.pending_spans
            .lock()
            .unwrap()
            .insert(span.span_id.clone(), span.clone());
    }

    async fn on_span_end(&self, span: &Span) {
        // Remove from pending, add to the completed buffer awaiting flush.
        self.pending_spans.lock().unwrap().remove(&span.span_id);

        let should_flush = {
            let mut buf = self.completed_spans.lock().unwrap();
            buf.push(span.clone());
            self.batch_size > 0 && buf.len() >= self.batch_size
        };

        // Eager flush once the batch fills — so a long-running collector
        // doesn't buffer every span until session end. The real export
        // happens in export_batch() -> exporter.export().
        if should_flush {
            if let Err(e) = self.export_batch().await {
                tracing::warn!("OTLP eager flush failed: {e}");
            }
        }
    }

    async fn on_event(&self, event: &TraceEvent, span_id: &str) {
        // Events are embedded in spans via TraceContext, not separately routed.
        // When the span ends, all its events are converted together (see
        // build_otlp_json_payload -> events_to_otlp).
        tracing::debug!(
            "OTEL: event {} kind={} in span {}",
            event.name,
            event_kind_to_otel_name(&event.kind),
            span_id
        );
    }

    async fn flush(&self) -> Result<(), String> {
        // Real export: drain the completed buffer through the exporter.
        tracing::debug!(
            "OTEL: flush — {} completed spans, {} pending",
            self.completed_count(),
            self.pending_count()
        );
        self.export_batch().await
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TraceContext;
    use crate::event::TraceEvent;
    use crate::span::Span;
    use std::sync::Arc;

    #[test]
    fn test_otel_config_grpc() {
        let config = OtlpConfig::grpc("http://localhost:4317", "oneai-agent");
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.protocol, OtlpProtocol::Grpc);
        assert_eq!(config.service_name, "oneai-agent");
    }

    #[test]
    fn test_otel_config_http() {
        let config = OtlpConfig::http("http://localhost:4318", "oneai-agent");
        assert_eq!(config.protocol, OtlpProtocol::Http);
    }

    #[test]
    fn test_otel_config_with_attribute() {
        let config = OtlpConfig::grpc("http://localhost:4317", "oneai")
            .with_attribute("deployment.environment", "production")
            .with_attribute("service.version", "0.1.0");
        assert_eq!(
            config.resource_attributes.get("deployment.environment"),
            Some(&"production".to_string())
        );
        assert_eq!(
            config.resource_attributes.get("service.version"),
            Some(&"0.1.0".to_string())
        );
    }

    #[test]
    fn test_span_kind_to_otel() {
        assert_eq!(span_kind_to_otel(&SpanKind::SESSION), "INTERNAL");
        assert_eq!(span_kind_to_otel(&SpanKind::AGENT), "INTERNAL");
        assert_eq!(span_kind_to_otel(&SpanKind::TOOL), "CLIENT");
        assert_eq!(span_kind_to_otel(&SpanKind::LLM), "CLIENT");
        assert_eq!(span_kind_to_otel(&SpanKind::RETRIEVER), "CLIENT");
        assert_eq!(span_kind_to_otel(&SpanKind::APPROVAL), "SERVER");
    }

    #[test]
    fn test_span_status_to_otel() {
        assert_eq!(span_status_to_otel(&SpanStatus::Ok), ("Ok", None));
        assert_eq!(
            span_status_to_otel(&SpanStatus::Error),
            ("Error", Some("operation failed"))
        );
        assert_eq!(
            span_status_to_otel(&SpanStatus::Cancelled),
            ("Error", Some("operation cancelled"))
        );
    }

    #[test]
    fn test_event_kind_to_otel_name() {
        assert_eq!(
            event_kind_to_otel_name(&EventKind::Thought),
            "agent.thought"
        );
        assert_eq!(event_kind_to_otel_name(&EventKind::Action), "agent.action");
        assert_eq!(
            event_kind_to_otel_name(&EventKind::Observation),
            "agent.observation"
        );
        assert_eq!(
            event_kind_to_otel_name(&EventKind::InferenceEnd),
            "llm.inference.end"
        );
        assert_eq!(event_kind_to_otel_name(&EventKind::ToolCall), "tool.call");
        assert_eq!(
            event_kind_to_otel_name(&EventKind::ApprovalRequest),
            "approval.request"
        );
        assert_eq!(
            event_kind_to_otel_name(&EventKind::MemoryRetrieve),
            "memory.retrieve"
        );
    }

    #[test]
    fn test_span_to_otel_json() {
        let mut span = Span::new(SpanKind::LLM, "inference", None);
        span.set_attribute("llm.model", serde_json::json!("gpt-4"));
        span.set_attribute("llm.token_count", serde_json::json!(1500));
        span.add_event(TraceEvent::thought("I need to calculate this"));
        span.end(SpanStatus::Ok);

        let json = OtlpCollector::span_to_otel_json(&span);

        assert_eq!(json["kind"], "CLIENT");
        assert_eq!(json["name"], "inference");
        assert_eq!(json["status"]["code"], "Ok");
        assert!(json["attributes"].get("oneai.llm.model").is_some());
        assert!(json["attributes"].get("oneai.llm.token_count").is_some());
        assert_eq!(json["events"][0]["name"], "agent.thought");
    }

    #[tokio::test]
    async fn test_otlp_collector_basic() {
        let config = OtlpConfig::grpc("http://localhost:4317", "oneai-test");
        let collector = OtlpCollector::new(config);

        let span = Span::new(SpanKind::SESSION, "session", None);
        collector.on_span_start(&span).await;

        assert_eq!(collector.pending_count(), 1);
        assert_eq!(collector.completed_count(), 0);

        let mut ended_span = span.clone();
        ended_span.end(SpanStatus::Ok);
        collector.on_span_end(&ended_span).await;

        assert_eq!(collector.pending_count(), 0);
        assert_eq!(collector.completed_count(), 1);
    }

    #[tokio::test]
    async fn test_otlp_collector_with_context() {
        let config = OtlpConfig::grpc("http://localhost:4317", "oneai-test");
        let collector = Arc::new(OtlpCollector::new(config));

        let ctx = TraceContext::new(collector.clone());
        let session_span = ctx.enter_span(SpanKind::SESSION, "session", None);
        ctx.set_attribute("session.id", serde_json::json!("test_otel_123"));

        let agent_span = ctx.enter_span(SpanKind::AGENT, "react_loop", None);
        ctx.log_event(
            EventKind::Thought,
            "agent.thought",
            HashMap::from([(
                "input.message".to_string(),
                serde_json::json!("What is OTEL?"),
            )]),
        );
        ctx.exit_span(&agent_span, SpanStatus::Ok);
        ctx.exit_span(&session_span, SpanStatus::Ok);

        let tree = ctx.build_tree();
        assert_eq!(tree.root_span.kind, SpanKind::SESSION);

        // Verify the collector received spans
        assert_eq!(collector.completed_count(), 0); // on_span_end may not have been called for nested spans

        // Verify OTEL JSON conversion
        let otel_json = OtlpCollector::span_to_otel_json(&tree.root_span);
        assert_eq!(otel_json["kind"], "INTERNAL");
        assert_eq!(otel_json["name"], "session");
    }

    #[tokio::test]
    async fn test_otlp_collector_flush() {
        let config = OtlpConfig::grpc("http://localhost:4317", "oneai-test");
        let collector = OtlpCollector::new(config);

        let result = collector.flush().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_otlp_collector_export_json() {
        let config = OtlpConfig::grpc("http://localhost:4317", "oneai-test");
        let collector = OtlpCollector::new(config);

        // Directly add a completed span
        let mut span = Span::new(SpanKind::SESSION, "session", None);
        span.set_attribute("session.id", serde_json::json!("export_test"));
        span.end(SpanStatus::Ok);
        collector.on_span_end(&span).await;

        // Now export JSON — should contain the completed span
        let json = collector.export_json().unwrap();
        assert!(json.contains("session") || json.contains("SESSION") || json.contains("Ok"));
    }

    #[test]
    fn test_nested_span_otel_conversion() {
        let mut root = Span::new(SpanKind::SESSION, "session", None);
        let mut agent = Span::new(SpanKind::AGENT, "react_loop", Some(&root.span_id));
        agent.set_attribute("agent.paradigm", serde_json::json!("react"));
        agent.add_event(TraceEvent::action(
            "calculator",
            &serde_json::json!({"expr": "2+2"}),
        ));
        agent.end(SpanStatus::Ok);

        let mut tool = Span::new(SpanKind::TOOL, "tool.calculator", Some(&agent.span_id));
        tool.set_attribute("tool.name", serde_json::json!("calculator"));
        tool.add_event(TraceEvent::observation(true, "4"));
        tool.end(SpanStatus::Ok);

        agent.add_child(tool);
        root.add_child(agent);
        root.end(SpanStatus::Ok);

        let otel_root = OtlpCollector::span_to_otel_json(&root);

        // Verify nested structure
        assert_eq!(otel_root["kind"], "INTERNAL");
        assert_eq!(otel_root["children"][0]["kind"], "INTERNAL");
        assert_eq!(otel_root["children"][0]["children"][0]["kind"], "CLIENT");

        // Verify attributes are prefixed with oneai namespace
        assert!(otel_root["children"][0]["attributes"]
            .get("oneai.agent.paradigm")
            .is_some());
        assert!(otel_root["children"][0]["children"][0]["attributes"]
            .get("oneai.tool.name")
            .is_some());

        // Verify events are mapped
        assert_eq!(
            otel_root["children"][0]["events"][0]["name"],
            "agent.action"
        );
        assert_eq!(
            otel_root["children"][0]["children"][0]["events"][0]["name"],
            "agent.observation"
        );
    }

    // ─── Real-export regression tests (gap-analysis #4) ───────────────
    //
    // These prove spans actually leave the collector via the exporter on
    // flush — the old implementation only buffered locally. They use
    // InMemoryOtlpExporter as a capturing test double so no live OTEL
    // collector is needed.

    #[test]
    fn test_normalize_traces_url() {
        assert_eq!(
            normalize_traces_url("http://localhost:4318"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            normalize_traces_url("http://localhost:4318/"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            normalize_traces_url("http://localhost:4318/v1/traces"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            normalize_traces_url("https://collector.example.com/v1/traces/"),
            "https://collector.example.com/v1/traces"
        );
    }

    #[tokio::test]
    async fn otlp_collector_exports_spans_via_exporter_on_flush() {
        let exporter = Arc::new(InMemoryOtlpExporter::new());
        let config = OtlpConfig::http("http://localhost:4318", "oneai-test");
        let collector =
            OtlpCollector::with_exporter(config, exporter.clone() as Arc<dyn OtlpExporter>);

        // Empty flush → Ok, no batch captured.
        collector.flush().await.unwrap();
        assert_eq!(exporter.batch_count(), 0);

        let mut span = Span::new(SpanKind::SESSION, "session", None);
        span.set_attribute("session.id", serde_json::json!("abc"));
        span.end(SpanStatus::Ok);
        collector.on_span_end(&span).await;

        assert_eq!(collector.completed_count(), 1);
        assert_eq!(
            exporter.total_spans(),
            0,
            "span must not be exported before flush"
        );

        collector.flush().await.unwrap();

        assert_eq!(exporter.batch_count(), 1);
        assert_eq!(exporter.total_spans(), 1);
        assert_eq!(collector.completed_count(), 0, "buffer drained after flush");

        let captured = exporter.exported_spans();
        assert_eq!(captured[0].name, "session");
    }

    #[tokio::test]
    async fn otlp_collector_eager_flush_at_batch_size() {
        let exporter = Arc::new(InMemoryOtlpExporter::new());
        let config = OtlpConfig::http("http://localhost:4318", "oneai-test");
        let collector =
            OtlpCollector::with_exporter(config, exporter.clone() as Arc<dyn OtlpExporter>)
                .with_batch_size(2);

        for i in 0..2 {
            let mut s = Span::new(SpanKind::AGENT, &format!("agent_{i}"), None);
            s.end(SpanStatus::Ok);
            collector.on_span_end(&s).await;
        }

        // batch_size reached → eager flush without an explicit flush() call.
        assert_eq!(exporter.total_spans(), 2);
        assert_eq!(collector.completed_count(), 0);
    }

    #[tokio::test]
    async fn otlp_collector_flush_error_surfaces_not_panics() {
        /// An exporter that always fails — proves flush() surfaces the error
        /// instead of silently swallowing it (the old stub behavior).
        struct AlwaysFailExporter;
        #[async_trait]
        impl OtlpExporter for AlwaysFailExporter {
            async fn export(&self, _batch: ExportBatch) -> Result<(), String> {
                Err("simulated collector down".to_string())
            }
        }

        let config = OtlpConfig::http("http://localhost:4318", "oneai-test");
        let collector = OtlpCollector::with_exporter(
            config,
            Arc::new(AlwaysFailExporter) as Arc<dyn OtlpExporter>,
        );
        let mut span = Span::new(SpanKind::SESSION, "session", None);
        span.end(SpanStatus::Ok);
        collector.on_span_end(&span).await;

        let result = collector.flush().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("simulated collector down"));
        // Buffer is still drained — a failed export doesn't re-buffer the batch.
        assert_eq!(collector.completed_count(), 0);
    }

    #[test]
    fn test_build_otlp_json_payload_structure() {
        // Root + child, with attributes and an event — exercise the full
        // OTLP/JSON wire-shape conversion that HttpOtlpExporter POSTs.
        let mut root = Span::new(SpanKind::SESSION, "session", None);
        root.set_attribute("session.id", serde_json::json!("s1"));
        root.end(SpanStatus::Ok);

        let mut child = Span::new(SpanKind::TOOL, "tool.calc", Some(&root.span_id));
        child.set_attribute("tool.name", serde_json::json!("calculator"));
        child.set_attribute("tool.count", serde_json::json!(42));
        child.add_event(TraceEvent::observation(true, "4"));
        child.end(SpanStatus::Error);

        let batch = ExportBatch {
            service_name: "oneai-test".to_string(),
            resource_attributes: HashMap::from([(
                "deployment.environment".to_string(),
                "ci".to_string(),
            )]),
            spans: vec![root, child],
        };
        let payload = build_otlp_json_payload(&batch);

        // resourceSpans[0].resource.attributes contains service.name + the user attr.
        let resource = &payload["resourceSpans"][0]["resource"]["attributes"];
        assert_eq!(resource[0]["key"], "service.name");
        assert_eq!(resource[0]["value"]["stringValue"], "oneai-test");
        assert_eq!(resource[1]["key"], "deployment.environment");

        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(spans.as_array().unwrap().len(), 2, "root + child");

        // traceId is 32 hex, spanId 16 hex.
        let trace_id = spans[0]["traceId"].as_str().unwrap();
        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
        let span_id = spans[0]["spanId"].as_str().unwrap();
        assert_eq!(span_id.len(), 16);

        // Root has no parentSpanId; child has one matching root's spanId.
        assert!(spans[0].get("parentSpanId").is_none() || spans[0]["parentSpanId"].is_null());
        assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
        // Child shares the root's traceId (same trace).
        assert_eq!(spans[1]["traceId"], spans[0]["traceId"]);

        // Kind codes: SESSION→INTERNAL=1, TOOL→CLIENT=3.
        assert_eq!(spans[0]["kind"], 1);
        assert_eq!(spans[1]["kind"], 3);

        // Status: root Ok=1, child Error=2 with a message.
        assert_eq!(spans[0]["status"]["code"], 1);
        assert_eq!(spans[1]["status"]["code"], 2);
        assert!(spans[1]["status"].get("message").is_some());

        // Attributes are OneAI-namespaced + typed anyValues. HashMap order is
        // nondeterministic, so look up by key rather than position.
        let attrs = spans[1]["attributes"].as_array().unwrap();
        let find_attr = |key: &str| -> &serde_json::Value {
            attrs
                .iter()
                .find(|a| a["key"] == key)
                .unwrap_or_else(|| panic!("missing attribute {key}"))
                .get("value")
                .unwrap()
        };
        assert_eq!(find_attr("oneai.tool.name")["stringValue"], "calculator");
        assert_eq!(find_attr("oneai.tool.count")["intValue"], "42");

        // Event maps to OTLP event with timeUnixNano + name.
        assert_eq!(spans[1]["events"][0]["name"], "agent.observation");
        assert!(spans[1]["events"][0]["timeUnixNano"].is_string());
    }

    #[tokio::test]
    async fn otlp_collector_with_default_http_exporter_constructs() {
        // `new` (default path used by AppBuilder::trace_otel) must construct
        // a real HttpOtlpExporter-backed collector without panicking.
        let config = OtlpConfig::http("http://localhost:4318", "oneai-agent");
        let collector = OtlpCollector::new(config);
        // Empty flush is a no-op (no network).
        collector.flush().await.unwrap();
    }
}
