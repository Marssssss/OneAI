//! CLI WASM module management commands.
//!
//! Subcommands:
//!   oneai wasm list       — List loaded WASM modules
//!   oneai wasm load       — Load a WASM module from file
//!   oneai wasm run        — Execute a loaded WASM module
//!   oneai wasm health     — Check module health
//!   oneai wasm unload     — Unload a WASM module
//!   oneai wasm stats      — Show resource monitor statistics

use std::path::PathBuf;
use std::sync::Arc;

use oneai_wasm::{WasmRuntime, WasmRuntimeConfig, WasmModuleRegistry, WasmResourceMonitor, WasmModuleSource, WasiConfig};

/// List all loaded WASM modules.
pub fn cmd_wasm_list() {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(async {
        let runtime = Arc::new(WasmRuntime::with_defaults().expect("WASM runtime creation failed"));
        let registry = WasmModuleRegistry::new(runtime);
        let modules = registry.list().await;

        if modules.is_empty() {
            println!("No WASM modules loaded.");
            return;
        }

        println!("Loaded WASM modules:");
        println!("{:<20} {:<15} {:<12} {:<20}", "Name", "Source", "Health", "Version");
        println!("{}", "-".repeat(67));
        for entry in modules {
            let source = match entry.source() {
                WasmModuleSource::File { path } => format!("File: {}", path.display()),
                WasmModuleSource::Bytes { .. } => "Bytes".to_string(),
                WasmModuleSource::Url { url } => format!("URL: {}", url),
                WasmModuleSource::Builtin { name } => format!("Builtin: {}", name),
                _ => "Other".to_string(),
            };
            let health = match entry.health() {
                oneai_wasm::WasmModuleHealth::Healthy => "Healthy".to_string(),
                oneai_wasm::WasmModuleHealth::Degraded { reason } => format!("Degraded: {}", reason),
                oneai_wasm::WasmModuleHealth::Unhealthy { reason } => format!("Unhealthy: {}", reason),
                oneai_wasm::WasmModuleHealth::Unknown => "Unknown".to_string(),
                _ => "Other".to_string(),
            };
            let version = entry.version().as_ref()
                .map(|v| v.version().to_string())
                .unwrap_or_else(|| "-".to_string());

            println!("{:<20} {:<15} {:<12} {:<20}", entry.name(), source, health, version);
        }
    });
}

/// Load a WASM module from file.
pub fn cmd_wasm_load(name: &str, file: &str) {
    let path = PathBuf::from(file);
    if !path.exists() {
        println!("Error: File '{}' not found.", file);
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(async {
        let runtime = Arc::new(WasmRuntime::with_defaults().expect("WASM runtime creation failed"));
        let registry = WasmModuleRegistry::new(runtime);

        match registry.register_file(name, &path).await {
            Ok(_tool) => {
                println!("Module '{}' loaded successfully from '{}'.", name, file);
                println!("Run 'oneai wasm list' to see all loaded modules.");
            }
            Err(e) => {
                println!("Error loading module '{}': {}", name, e);
            }
        }
    });
}

/// Execute a loaded WASM module with JSON input.
pub fn cmd_wasm_run(name: &str, input: Option<&str>, input_file: Option<&str>) {
    let input_json = match (input, input_file) {
        (Some(json), None) => serde_json::from_str(json)
            .unwrap_or_else(|_| serde_json::Value::String(json.to_string())),
        (None, Some(path)) => {
            let content = std::fs::read_to_string(path)
                .unwrap_or_else(|e| {
                    println!("Error reading input file '{}': {}", path, e);
                    std::process::exit(1);
                });
            serde_json::from_str(&content)
                .unwrap_or_else(|_| serde_json::Value::String(content))
        }
        (None, None) => serde_json::json!({}),
        (Some(_), Some(_)) => {
            println!("Error: Cannot specify both --input and --input-file.");
            return;
        }
    };

    println!("Executing module '{}' with input: {}", name, input_json);

    // Note: Running a WASM module requires it to be already loaded in a runtime.
    // For the CLI, we'd need a persistent runtime or a way to load + execute in one step.
    // Currently, this is a placeholder — actual execution requires the module to be
    // registered in a WasmModuleRegistry that's connected to a running App.
    println!("Note: WASM module execution requires a running App instance.");
    println!("Use 'oneai run' or 'oneai chat' to execute WASM tools within an agent session.");
}

/// Check WASM module health.
pub fn cmd_wasm_health(name: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(async {
        let runtime = Arc::new(WasmRuntime::with_defaults().expect("WASM runtime creation failed"));
        let registry = WasmModuleRegistry::new(runtime);

        if let Some(name) = name {
            let health = registry.check_health(name).await;
            let health_str = match &health {
                oneai_wasm::WasmModuleHealth::Healthy => "✅ Healthy".to_string(),
                oneai_wasm::WasmModuleHealth::Degraded { reason } => format!("⚠️ Degraded: {}", reason),
                oneai_wasm::WasmModuleHealth::Unhealthy { reason } => format!("❌ Unhealthy: {}", reason),
                oneai_wasm::WasmModuleHealth::Unknown => "❓ Unknown".to_string(),
                _ => "❓ Other".to_string(),
            };
            println!("Module '{}' health: {}", name, health_str);
        } else {
            let modules = registry.list().await;
            if modules.is_empty() {
                println!("No WASM modules loaded.");
                return;
            }

            println!("Module health status:");
            for entry in modules {
                let health_str = match entry.health() {
                    oneai_wasm::WasmModuleHealth::Healthy => "✅ Healthy".to_string(),
                    oneai_wasm::WasmModuleHealth::Degraded { reason } => format!("⚠️ Degraded: {}", reason),
                    oneai_wasm::WasmModuleHealth::Unhealthy { reason } => format!("❌ Unhealthy: {}", reason),
                    oneai_wasm::WasmModuleHealth::Unknown => "❓ Unknown".to_string(),
                    _ => "❓ Other".to_string(),
                };
                println!("  {}: {}", entry.name(), health_str);
            }
        }
    });
}

/// Unload a WASM module.
pub fn cmd_wasm_unload(name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(async {
        let runtime = Arc::new(WasmRuntime::with_defaults().expect("WASM runtime creation failed"));
        let registry = WasmModuleRegistry::new(runtime);

        match registry.unload(name).await {
            Ok(_) => println!("Module '{}' unloaded successfully.", name),
            Err(e) => println!("Error unloading module '{}': {}", name, e),
        }
    });
}

/// Show resource monitor statistics.
pub fn cmd_wasm_stats() {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(async {
        let monitor = Arc::new(WasmResourceMonitor::new());

        let metrics = monitor.all_metrics().await;
        let total_calls = monitor.total_calls().await;
        let total_fuel = monitor.total_fuel_consumed().await;
        let total_errors = monitor.total_errors().await;

        println!("WASM Resource Monitor Statistics");
        println!("{}", "=".repeat(50));
        println!("Total calls: {}", total_calls);
        println!("Total fuel consumed: {}", total_fuel);
        println!("Total errors: {}", total_errors);
        println!();

        if metrics.is_empty() {
            println!("No modules have been executed yet.");
            return;
        }

        println!("Per-module metrics:");
        println!("{:<20} {:>10} {:>15} {:>15} {:>10}", "Module", "Calls", "Fuel Used", "Avg Time (ms)", "Errors");
        println!("{}", "-".repeat(70));
        for m in metrics {
            println!("{:<20} {:>10} {:>15} {:>15.1} {:>10}",
                m.module_name(), m.total_calls(), m.total_fuel_consumed(),
                m.avg_execution_time_ms(), m.total_errors());
        }
    });
}
