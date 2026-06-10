//! OneAI CLI Demo — interactive REPL showcasing the full framework pipeline.
//!
//! This demo demonstrates:
//! - Building an App with tools, memory, RAG, and persistence
//! - Sending messages and retrieving memory context
//! - Executing tools (calculator, file operations)
//! - Running workflows
//! - Saving and loading checkpoints
//!
//! Usage:
//!   cargo run -p oneai-cli-demo

use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_tool::{CalculatorTool, FileReadTool, FileWriteTool, ShellTool};
use oneai_memory::MemoryManager;
use oneai_rag::{Document, DocumentIndex, ChunkingStrategy};
use oneai_workflow::{WorkflowConfig, StepConfig};
use oneai_persistence::FilePersistence;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("╔══════════════════════════════════════╗");
    println!("║   OneAI Framework — CLI Demo        ║");
    println!("║   Phase 1–6 + Trace Showcase         ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    // ─── 1. Build the App (with in-memory trace) ──────────────────────
    println!("🔧 Building OneAI App with trace...");
    let app = AppBuilder::new()
        .auto_approval_gate()   // Auto-approve for demo purposes
        .default_parser()       // 3-layer parser
        .memory_manager(Arc::new(MemoryManager::with_config(oneai_memory::MemoryManagerConfig {
            stm_window_size: 10,
            compression_threshold_tokens: 2000,
            compression_keep_recent_turns: 6,
            evict_to_ltm: true,
        })))
        .persistence(Arc::new(FilePersistence::new("/tmp/oneai_demo_checkpoints")))
        .trace_in_memory()      // Enable trajectory logging
        .build()
        .expect("App build should succeed");

    println!("   ✓ App built successfully (provider: none, tools: pending)");
    println!();

    // ─── 2. Register Tools ────────────────────────────────────────────────
    println!("🛠  Registering tools...");
    app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
    app.register_tool(Arc::new(FileReadTool::new())).await.unwrap();
    app.register_tool(Arc::new(FileWriteTool::new())).await.unwrap();
    app.register_tool(Arc::new(ShellTool::new())).await.unwrap();
    let tools = app.tool_executor().list_tools().await;
    println!("   ✓ Registered {} tools: {}", tools.len(), tools.join(", "));
    println!();

    // ─── 3. Create a Session ────────────────────────────────────────────
    println!("💬 Creating session...");
    let mut session = app.create_session();
    println!("   ✓ Session ID: {}", session.session_id());
    println!();

    // ─── 4. Memory Demo ─────────────────────────────────────────────────
    println!("🧠 Memory Demo");
    println!("─────────────");

    // Add messages to memory
    session.send_user_message("Rust is a systems programming language focused on safety and performance").await.unwrap();
    session.send_user_message("The OneAI framework uses ReAct, Plan, and Reflection paradigms").await.unwrap();
    session.send_user_message("Tokio is the async runtime used throughout OneAI").await.unwrap();
    println!("   → Added 3 messages to memory");

    // Retrieve from memory
    let results = session.retrieve_memory("programming", 5).await.unwrap();
    println!("   → Retrieved {} memories about 'programming'", results.len());
    for entry in &results {
        println!("      • [{}] {}", entry.id, entry.content.chars().take(60).collect::<String>());
    }

    // Retrieve more specific query
    let results2 = session.retrieve_memory("OneAI", 3).await.unwrap();
    println!("   → Retrieved {} memories about 'OneAI'", results2.len());
    println!();

    // ─── 5. Tool Execution Demo ──────────────────────────────────────────
    println!("⚡ Tool Execution Demo");
    println!("─────────────────────");

    // Calculator
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3*4"})).await.unwrap();
    println!("   calculator(2+3*4) → {} (success: {})", result.content, result.success);

    let result = session.execute_tool("calculator", serde_json::json!({"expression": "100/5"})).await.unwrap();
    println!("   calculator(100/5) → {} (success: {})", result.content, result.success);

    let result = session.execute_tool("calculator", serde_json::json!({"expression": "(10+5)*3"})).await.unwrap();
    println!("   calculator((10+5)*3) → {} (success: {})", result.content, result.success);

    // Calculator error case
    let result = session.execute_tool("calculator", serde_json::json!({"expression": ""})).await.unwrap();
    println!("   calculator('') → error: {} (success: {})", result.error.unwrap_or_default(), result.success);

    // Shell (with auto-approve gate)
    let result = session.execute_tool("shell", serde_json::json!({"command": "echo 'Hello from OneAI!'"})).await.unwrap();
    println!("   shell(echo 'Hello from OneAI!') → success: {}", result.success);
    println!();

    // ─── 6. Workflow Demo ──────────────────────────────────────────────
    println!("🔄 Workflow Demo");
    println!("──────────────────");

    let config = WorkflowConfig::new("calc_pipeline", vec![
        StepConfig {
            id: "add".to_string(),
            description: "Calculate 15 + 25".to_string(),
            depends_on: vec![],
            tool: Some("calculator".to_string()),
            tool_args: Some(serde_json::json!({"expression": "15+25"})),
            prompt: None,
            requires_approval: false,
            timeout_secs: Some(10),
            retry_policy: None,
            metadata: std::collections::HashMap::new(),
        },
        StepConfig {
            id: "multiply".to_string(),
            description: "Calculate 40 * 3".to_string(),
            depends_on: vec!["add".to_string()],
            tool: Some("calculator".to_string()),
            tool_args: Some(serde_json::json!({"expression": "40*3"})),
            prompt: None,
            requires_approval: false,
            timeout_secs: Some(10),
            retry_policy: None,
            metadata: std::collections::HashMap::new(),
        },
        StepConfig {
            id: "final".to_string(),
            description: "Calculate 120 / 6".to_string(),
            depends_on: vec!["multiply".to_string()],
            tool: Some("calculator".to_string()),
            tool_args: Some(serde_json::json!({"expression": "120/6"})),
            prompt: None,
            requires_approval: false,
            timeout_secs: Some(10),
            retry_policy: None,
            metadata: std::collections::HashMap::new(),
        },
    ]);

    println!("   → Workflow: add(15+25) → multiply(40*3) → final(120/6)");
    let result = session.execute_workflow(&config).await;
    match result {
        Ok(wf_result) => {
            println!("   ✓ Workflow completed (success: {})", wf_result.success);
            println!("   ✓ Total time: {}ms", wf_result.total_time_ms);
            for (step_id, step_result) in &wf_result.step_results {
                println!("      • {} → status: {:?}, output: {}",
                    step_id, step_result.status,
                    step_result.output.as_deref().unwrap_or("none"));
            }
        }
        Err(e) => {
            println!("   ✗ Workflow failed: {}", e);
        }
    }
    println!();

    // ─── 7. Checkpoint Demo ────────────────────────────────────────────
    println!("💾 Checkpoint Demo");
    println!("───────────────────");

    let checkpoint_id = session.save_checkpoint().await.unwrap();
    println!("   ✓ Saved checkpoint: {}", checkpoint_id);

    // List checkpoints
    if let Some(persistence) = app.persistence() {
        use oneai_core::traits::StatePersistence;
        let checkpoints = persistence.list_checkpoints().await.unwrap();
        println!("   ✓ Found {} checkpoints", checkpoints.len());
        for cp in &checkpoints {
            println!("      • {} — {}", cp.id, cp.description.chars().take(50).collect::<String>());
        }
    }
    println!();

    // ─── 8. RAG Demo ──────────────────────────────────────────────────
    println!("🔍 RAG Demo");
    println!("─────────────");

    // Create a simple RAG index
    let vector_store = Arc::new(oneai_memory::ThreadSafeEmbeddedVectorStore::new());
    let mut rag_index = DocumentIndex::with_defaults(vector_store);

    // Add documents
    let mut doc1 = Document::with_id("rust_intro", "Rust is a systems programming language that prioritizes safety, speed, and concurrency. It was designed by Mozilla and first released in 2015.");
    doc1.chunk(&ChunkingStrategy::SentenceBoundary { max_chunk_size: 200 });
    let chunk_ids = rag_index.add_document(doc1).unwrap();
    println!("   → Added 'rust_intro' document with {} chunks", chunk_ids.len());

    let mut doc2 = Document::with_id("tokio_guide", "Tokio is an asynchronous runtime for the Rust programming language. It provides the building blocks needed for writing network applications, including async I/O, timers, and task scheduling.");
    doc2.chunk(&ChunkingStrategy::SentenceBoundary { max_chunk_size: 200 });
    let chunk_ids2 = rag_index.add_document(doc2).unwrap();
    println!("   → Added 'tokio_guide' document with {} chunks", chunk_ids2.len());

    // Keyword search
    let results = rag_index.search_by_keyword("programming language", 5);
    println!("   → Keyword search 'programming language': {} results", results.len());
    for r in &results {
        println!("      • [{}] score={} — {}", r.chunk.document_id, r.score, r.chunk.content.chars().take(60).collect::<String>());
    }
    println!();

    // ─── Summary ──────────────────────────────────────────────────────
    println!("╔══════════════════════════════════════╗");
    println!("║   Demo Complete!                     ║");
    println!("║   Framework Status:                  ║");
    println!("║   • 18 crates, 211 tests             ║");
    println!("║   • 7 phases (1–6 + Trace)           ║");
    println!("║   • Platform UI + UniFFI + Trace     ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    // ─── 9. Trace Tree Export ──────────────────────────────────────────
    println!("📊 Trace Tree Export");
    println!("───────────────────────");

    // End the session span
    session.end_session(oneai_trace::SpanStatus::Ok);

    // Build and export the trace tree
    if let Some(tree) = session.build_trace_tree() {
        println!("   ✓ Trace tree built:");
        println!("      • Total spans: {}", tree.span_count());
        println!("      • Total events: {}", tree.event_count());
        println!("      • Root span: {:?}", tree.root_span.kind);
        println!("      • Root status: {:?}", tree.root_span.status);
        println!();

        // Print metrics summary
        println!("   ✓ Metrics:");
        println!("      • Success rate: {:.1}%", tree.metrics.success_rate * 100.0);
        println!("      • Tool calls: {}", tree.metrics.tool_call_count);
        println!("      • Tool success rate: {:.1}%", tree.metrics.tool_success_rate * 100.0);
        println!("      • Checkpoint saves: {}", tree.metrics.checkpoint_count);
        println!("      • Session duration: {}ms", tree.metrics.total_session_duration_ms);
        println!();

        // Export full JSON trace
        let json = tree.to_json().unwrap();
        let trace_path = "/tmp/oneai_demo_trace.json";
        tree.to_file(std::path::Path::new(trace_path)).unwrap();
        println!("   ✓ Trace exported to: {}", trace_path);
        println!("   ✓ JSON size: {} bytes", json.len());

        // Print a snippet of the trace JSON (first few lines)
        let snippet: String = json.lines().take(15).collect::<Vec<_>>().join("\n");
        println!();
        println!("   JSON snippet:");
        println!("   ───────────────");
        for line in snippet.lines() {
            println!("   {}", line);
        }
        println!("   ... (full trace in {})", trace_path);
    } else {
        println!("   ✗ No trace context available (tracing was not enabled)");
    }
}