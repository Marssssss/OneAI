//! Version command — display OneAI version information.

/// Display version information.
pub fn cmd_version() {
    println!("oneai {}", env!("CARGO_PKG_VERSION"));
    println!("OneAI — Rust Agent Framework CLI");
    println!();
    println!("Architecture:");
    println!("  • Agent Loop: dynamic decision-making (Plan/ReAct/Reflect/Explore)");
    println!("  • DomainPack: 5-layer pluggable domain configuration");
    println!("  • StateGraph: cyclic agent execution with interrupt/resume");
    println!("  • Memory: STM ↔ LTM closed loop with reflection");
    println!("  • A2A Protocol: inter-agent communication SDK");
    println!("  • WASM Sandbox: Wasmtime runtime for safe tool execution");
    println!("  • OTEL Observability: OpenTelemetry traces + metrics");
    println!();
    println!("Built-in Domain Packs:");
    println!("  • coding — software development");
    println!("  • research — web search & analysis");
    println!("  • general — basic tasks");
    println!();
    println!("License: Apache-2.0");
    println!("Repository: https://github.com/oneai-project/oneai");
}
