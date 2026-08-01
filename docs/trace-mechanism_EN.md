# OneAI Trace Mechanism

> OpenInference-compatible traces + OTEL exporter — making every inference / tool call / sub-agent delegate observable, metricable, and exportable to external backends.

## Responsibility

Record the span tree of an agent run (inference, tool calls, sub-agent delegates, paradigm switches), assemble it into an OpenInference-compatible trace, compute metrics (success rate / tokens / tool-call counts), and export via OTEL to external backends. Traces are also the data source for the eval "efficiency axis" and SWE-bench.

## Usage

```rust
let app = AppBuilder::new().trace_in_memory().build()?;
// …run the agent…
if let Some(ctx) = session.trace_context() {
    let tree = ctx.build_tree();                  // assemble span tree + compute metrics
    println!("Success rate: {:.1}%", tree.metrics.success_rate * 100.0);
}
```

## Key types & files

| Item | Location |
|---|---|
| trace context + span assembly | `crates/oneai-trace/src/context.rs` |
| `Collector` | `crates/oneai-trace/src/collector.rs` |
| span / event | `crates/oneai-trace/src/event.rs` |
| `TraceMetrics` | `crates/oneai-trace/src/metrics.rs` |
| OTEL export (`OtlpCollector` + `OtelMetricsProvider`) | `crates/oneai-trace/src/{otel_exporter,otel_metrics}.rs` |
| emitter / noop | `crates/oneai-trace/src/{emitter,noop}.rs` |

## Further reading

- AgentLoop trace write-in points — see [CLAUDE.md — AgentLoop](../CLAUDE.md)
- Trace → eval efficiency axis — see [Eval mechanism](eval-mechanism_EN.md), [Multi-agent mechanism](multi-agent-mechanism_EN.md)
