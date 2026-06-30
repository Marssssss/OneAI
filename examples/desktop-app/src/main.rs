//! OneAI Desktop Demo — Platform-native approval gates on macOS/Windows/Linux.
//!
//! This demo demonstrates:
//! - Building an App with a desktop platform approval gate
//! - Auto-approve threshold: requests below the threshold are auto-approved
//! - High-risk requests are received by the bridge and processed
//! - On macOS: bridge can show NSAlert dialogs via run_loop()
//! - On Windows/Linux: bridge processes items via recv_blocking()
//!
//! Usage:
//!   cargo run -p oneai-desktop-demo

use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_core::RiskLevel;
use oneai_memory::MemoryManager;
use oneai_persistence::FilePersistence;
use oneai_tool::{CalculatorTool, FileReadTool, ShellTool};
use oneai_core::{InteractionRequest, InteractionResponse};
use oneai_platform_desktop::DesktopInteractionGateFactory;
use oneai_platform_desktop::DesktopInteractionBridge;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    println!("╔══════════════════════════════════════╗");
    println!("║   OneAI — Desktop Platform Demo     ║");
    println!("║   Native UI Approval Gates           ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    // ─── Detect Platform ────────────────────────────────────────────
    let platform = DesktopInteractionGateFactory::current_platform();
    println!("🖥  Detected platform: {:?}", platform);
    println!();

    // ─── Create Desktop Approval Gate ───────────────────────────────
    println!("🛡  Creating desktop interaction gate...");
    println!("   • Auto-approve threshold: {:?}", RiskLevel::Medium);
    println!("   • Requests with risk ≤ Medium → auto-approved");
    println!("   • Requests with risk > Medium → bridge processing");
    println!();

    // Create the platform-specific gate + bridge pair
    // Buffer size 16, threshold Medium (auto-approve below Medium)
    let (gate, bridge) = DesktopInteractionGateFactory::create(16, RiskLevel::Medium);

    // ─── Build the App ──────────────────────────────────────────────
    println!("🔧 Building OneAI App with desktop interaction gate...");
    let app = AppBuilder::new()
        .interaction_gate(Arc::new(gate))
        .default_parser()
        .memory_manager(Arc::new(MemoryManager::new()))
        .persistence(Arc::new(FilePersistence::new("/tmp/oneai_desktop_demo")))
        .build()
        .await
        .expect("App build should succeed");
    println!("   ✓ App built successfully");
    println!();

    // ─── Register Tools ─────────────────────────────────────────────
    println!("🛠  Registering tools...");
    app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
    app.register_tool(Arc::new(FileReadTool::new())).await.unwrap();
    app.register_tool(Arc::new(ShellTool::new())).await.unwrap();
    let tools = app.tool_executor().list_tools().await;
    println!("   ✓ Registered {} tools: {}", tools.len(), tools.join(", "));
    println!();

    // ─── Create Session ─────────────────────────────────────────────
    let session = app.create_session();
    println!("💬 Session ID: {}", session.session_id());
    println!();

    // ─── Demo: Auto-approved Tool (Low Risk) ───────────────────────
    println!("⚡ Demo 1: Auto-approved tool (Low Risk)");
    println!("─────────────────────────────────────────");

    // Calculator is Low-risk → auto-approved without any dialog
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3*4"})).await.unwrap();
    println!("   calculator(2+3*4) → {} (success: {}, auto-approved)", result.content, result.success);

    let result = session.execute_tool("calculator", serde_json::json!({"expression": "100/5"})).await.unwrap();
    println!("   calculator(100/5) → {} (success: {}, auto-approved)", result.content, result.success);
    println!();

    // ─── Demo: High-Risk Tool (Requires Approval via Bridge) ───────
    println!("⚠️  Demo 2: High-risk tool (Requires Approval via Bridge)");
    println!("─────────────────────────────────────────────────────────");
    println!("   Shell commands are HIGH risk → sent through bridge channel");
    println!("   The bridge receives the item and we approve it programmatically.");
    println!();

    // In a real macOS/Windows app, the bridge would be integrated with the
    // native UI event loop (NSApplication.main / Win32 message pump).
    // Here we run the bridge handler in a background task, auto-approving
    // items as they arrive. On macOS, you would call bridge.run_loop() instead.
    let bridge_task = tokio::spawn(async move {
        // Poll for pending items using try_recv in a loop
        loop {
            match bridge.try_recv() {
                Some(item) => {
                    let approval = match &item.request {
                        InteractionRequest::ToolApproval { approval } => approval,
                        _ => {
                            let _ = DesktopInteractionBridge::send_response(
                                item,
                                InteractionResponse::Proceed,
                            );
                            continue;
                        }
                    };
                    println!("   → Bridge received: tool={}, risk={:?}",
                        approval.tool_name, approval.risk_level);

                    // Format the request for display
                    let formatted = DesktopInteractionBridge::format_request(approval);
                    println!("   → Request details:\n{}", formatted);
                    println!("   → Auto-approving (for demo purposes)");

                    // Send approval response
                    DesktopInteractionBridge::send_response(item, InteractionResponse::Proceed).unwrap();
                }
                None => {
                    // No pending items — wait briefly
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            }
        }
    });

    // Give the bridge handler a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Shell tool → high risk → needs approval via bridge
    println!("   → Executing: shell(echo 'Approved by desktop gate')");
    let result = session.execute_tool("shell", serde_json::json!({"command": "echo 'Approved by desktop gate'"})).await;
    match result {
        Ok(output) => println!("   ✓ Result: success={}, content={}", output.success, output.content.trim()),
        Err(e) => println!("   ✗ Error: {}", e),
    }
    println!();

    // ─── Demo: Manual-Only Gate ─────────────────────────────────────
    println!("🚫 Demo 3: Manual-only gate (all requests need approval)");
    println!("───────────────────────────────────────────────────────────────");

    let (manual_gate, manual_bridge) = DesktopInteractionGateFactory::create_manual_only(16);
    let manual_app = AppBuilder::new()
        .interaction_gate(Arc::new(manual_gate))
        .default_parser()
        .build()
        .await
        .expect("Manual app build should succeed");

    manual_app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
    let manual_session = manual_app.create_session();

    println!("   → Even calculator needs approval with manual-only gate");

    let manual_bridge_task = tokio::spawn(async move {
        loop {
            match manual_bridge.try_recv() {
                Some(item) => {
                    let tool_name = match &item.request {
                        InteractionRequest::ToolApproval { approval } => approval.tool_name.clone(),
                        _ => "<non-tool point>".to_string(),
                    };
                    println!("   → Manual bridge: tool={}", tool_name);
                    DesktopInteractionBridge::send_response(item, InteractionResponse::Proceed).unwrap();
                }
                None => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            }
        }
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let result = manual_session.execute_tool("calculator", serde_json::json!({"expression": "7*8"})).await;
    match result {
        Ok(output) => println!("   ✓ Result: success={}, content={}", output.success, output.content),
        Err(e) => println!("   ✗ Error: {}", e),
    }
    println!();

    // ─── Summary ──────────────────────────────────────────────────────
    println!("╔══════════════════════════════════════╗");
    println!("║   Desktop Demo Complete!             ║");
    println!("║   Platform: {:?}                     ║", platform);
    println!("║   17 crates, 197 tests              ║");
    println!("║   Phase 6 — Platform UI + Bindings   ║");
    println!("╚══════════════════════════════════════╝");

    // Cleanup
    bridge_task.abort();
    manual_bridge_task.abort();
}