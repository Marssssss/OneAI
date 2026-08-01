# OneAI 轨迹日志（Trace）机制

> OpenInference 兼容轨迹 + OTEL 导出器，让每一轮推理 / 工具调用可观测、可算指标、可对接外部链路。

## 职责

记录 Agent 执行的 span 树（推理、工具调用、子 Agent 委托、范式切换），组装成 OpenInference 兼容轨迹，计算成功率 / token / 工具调用次数等指标，并能经 OTEL 导出到外部后端。轨迹也是评测「效率轴」与 SWE-bench 的数据来源。

## 用法

```rust
let app = AppBuilder::new().trace_in_memory().build()?;
// …运行 agent…
if let Some(ctx) = session.trace_context() {
    let tree = ctx.build_tree();                  // 组装 span 树 + 计算指标
    println!("成功率: {:.1}%", tree.metrics.success_rate * 100.0);
}
```

## 关键类型与文件

| 项 | 位置 |
|---|---|
| 轨迹上下文 + span 组装 | `crates/oneai-trace/src/context.rs` |
| `Collector` | `crates/oneai-trace/src/collector.rs` |
| span / 事件 | `crates/oneai-trace/src/event.rs` |
| `TraceMetrics` | `crates/oneai-trace/src/metrics.rs` |
| OTEL 导出（`OtlpCollector` + `OtelMetricsProvider`） | `crates/oneai-trace/src/{otel_exporter,otel_metrics}.rs` |
| emitter / noop | `crates/oneai-trace/src/{emitter,noop}.rs` |

## 深入阅读

- AgentLoop 写入轨迹的接入点见 [CLAUDE.md — AgentLoop](../CLAUDE.md)
- 轨迹→评测效率轴见 [评测机制](eval-mechanism.md)、[多 Agent 机制](multi-agent-mechanism.md)
