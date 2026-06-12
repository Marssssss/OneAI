//! OneAI Channel Demo — Direct ChannelApprovalGate usage with manual approval.
//!
//! This demo demonstrates:
//! - Building an App with ChannelApprovalGateWithThreshold via AppBuilder
//! - Manual approval flow: receive pending items from the channel, inspect, respond
//! - Different approval responses: Approve, Deny, Modify args
//! - Auto-approve threshold: low-risk requests bypass manual review
//!
//! The approval mechanism:
//! - ChannelApprovalGateWithThreshold sends ApprovalPendingItem through an mpsc channel
//! - The UI/test handler receives items and sends responses via the embedded oneshot channel
//! - item.response_tx.send(response) unblocks the agent's approval request
//!
//! Usage:
//!   cargo run -p oneai-channel-demo

use std::sync::Arc;

use oneai_app::AppBuilder;
use oneai_core::RiskLevel;
use oneai_memory::MemoryManager;
use oneai_tool::{CalculatorTool, ShellTool, ApprovalDecision};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    println!("╔══════════════════════════════════════╗");
    println!("║   OneAI — Channel Approval Demo      ║");
    println!("║   Manual Approval Flow               ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    // ─── Create ChannelApprovalGate via AppBuilder ──────────────────
    println!("🛡  Creating ChannelApprovalGate...");
    println!("   • Buffer size: 16");
    println!("   • Auto-approve threshold: Medium (Low → auto, Medium+ → manual)");
    println!();

    // AppBuilder::channel_approval_gate() returns (builder, Receiver<ApprovalPendingItem>)
    let (builder, mut receiver) = AppBuilder::new()
        .channel_approval_gate(16, RiskLevel::Medium);

    // ─── Build App ───────────────────────────────────────────────────
    println!("🔧 Building OneAI App with channel approval gate...");
    let app = builder
        .default_parser()
        .memory_manager(Arc::new(MemoryManager::new()))
        .build()
        .await
        .expect("App build should succeed");
    println!("   ✓ App built successfully");
    println!();

    // ─── Register Tools ──────────────────────────────────────────────
    app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
    app.register_tool(Arc::new(ShellTool::new())).await.unwrap();
    let tools = app.tool_executor().list_tools().await;
    println!("🛠  Registered {} tools: {}", tools.len(), tools.join(", "));
    println!();

    // ─── Demo 1: Auto-approved (Low Risk) ────────────────────────────
    println!("⚡ Demo 1: Auto-approved tool (Low Risk)");
    println!("─────────────────────────────────────────");
    println!("   Calculator is Low-risk → auto-approved without manual review");

    let session = app.create_session();
    let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+2"})).await.unwrap();
    println!("   calculator(2+2) → {} (success: {})", result.content, result.success);

    let result = session.execute_tool("calculator", serde_json::json!({"expression": "15*3"})).await.unwrap();
    println!("   calculator(15*3) → {} (success: {})", result.content, result.success);

    // Check: no pending requests in the channel (all auto-approved)
    assert!(receiver.try_recv().is_err(), "No pending items after auto-approved calls");
    println!("   ✓ No pending approval items (all auto-approved)");
    println!();

    // ─── Demo 2: Manual Approval — Approve ─────────────────────────
    println!("⚠️  Demo 2: Manual approval — Approve shell command");
    println!("──────────────────────────────────────────────────────");

    // Spawn a background handler that receives and approves requests
    let handler_task = tokio::spawn(async move {
        // Wait for a pending item
        while let Some(item) = receiver.recv().await {
            println!("   → Received approval request:");
            println!("      • Tool: {}", item.request.tool_name);
            println!("      • Risk: {:?}", item.request.risk_level);
            println!("      • Args: {}", item.request.args);
            println!("      • Justification: {}", item.request.justification);
            println!();
            println!("   → APPROVING request");

            // Send Approved response via the embedded oneshot channel
            item.response_tx.send(ApprovalDecision::approve()).unwrap();
        }
    });

    // Give the handler a moment to start listening
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Execute a high-risk tool (shell) — will wait for approval
    let result = session.execute_tool("shell", serde_json::json!({"command": "echo 'Hello OneAI!'"})).await.unwrap();
    println!("   ✓ Shell result: success={}, content={}", result.success, result.content.trim());
    println!();

    handler_task.abort();

    // ─── Demo 3: Deny Approval ──────────────────────────────────────
    println!("🚫 Demo 3: Denied approval");
    println!("──────────────────────────");

    // Build a new app with Low threshold (only Low risk auto-approved)
    let (builder2, mut receiver2) = AppBuilder::new()
        .channel_approval_gate(16, RiskLevel::Low);

    let app2 = builder2
        .default_parser()
        .memory_manager(Arc::new(MemoryManager::new()))
        .build()
        .await
        .expect("App build should succeed");

    app2.register_tool(Arc::new(ShellTool::new())).await.unwrap();
    let session2 = app2.create_session();

    // Handler that denies all requests
    let deny_handler = tokio::spawn(async move {
        while let Some(item) = receiver2.recv().await {
            println!("   → Received request for dangerous command:");
            println!("      • Tool: {}", item.request.tool_name);
            println!("      • Args: {}", item.request.args);
            println!("   → DENYING request");

            item.response_tx.send(ApprovalDecision::deny("Dangerous command rejected")).unwrap();
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let result = session2.execute_tool("shell", serde_json::json!({"command": "rm -rf /"})).await.unwrap();
    println!("   ✓ Shell denied: success={}, error={}", result.success, result.error.unwrap_or_default());
    println!();

    deny_handler.abort();

    // ─── Demo 4: Modified Approval ───────────────────────────────────
    println!("✏️  Demo 4: Modified approval (change args before executing)");
    println!("─────────────────────────────────────────────────────────");

    // Use a manual-only ChannelApprovalGate
    let (manual_gate, mut manual_receiver) = oneai_tool::ChannelApprovalGateWithThreshold::new_manual_only(16);
    let app3 = AppBuilder::new()
        .approval_gate(Arc::new(manual_gate))
        .default_parser()
        .build()
        .await
        .expect("Manual app build should succeed");

    app3.register_tool(Arc::new(ShellTool::new())).await.unwrap();
    let session3 = app3.create_session();

    // Handler that modifies args
    let modify_handler = tokio::spawn(async move {
        while let Some(item) = manual_receiver.recv().await {
            println!("   → Received request for 'cat /etc/passwd'");
            println!("   → MODIFYING args to safer command: 'echo Modified_by_gate'");

            item.response_tx.send(ApprovalDecision::modify(
                serde_json::json!({"command": "echo 'Modified by approval gate'"})
            )).unwrap();
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let result = session3.execute_tool("shell", serde_json::json!({"command": "cat /etc/passwd"})).await.unwrap();
    println!("   ✓ Modified result: success={}, content={}", result.success, result.content.trim());
    println!();

    modify_handler.abort();

    // ─── Demo 5: Manual-Only Gate (even calculator needs approval) ──
    println!("🔒 Demo 5: Manual-only gate (all requests need approval)");
    println!("─────────────────────────────────────────────");

    let (manual_gate2, mut manual_receiver2) = oneai_tool::ChannelApprovalGateWithThreshold::new_manual_only(16);
    let app4 = AppBuilder::new()
        .approval_gate(Arc::new(manual_gate2))
        .default_parser()
        .build()
        .await
        .expect("Manual app build should succeed");

    app4.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
    let session4 = app4.create_session();

    println!("   → Even calculator needs approval with manual-only gate");

    let approve_handler = tokio::spawn(async move {
        while let Some(item) = manual_receiver2.recv().await {
            println!("      • Tool: {}, Risk: {:?}", item.request.tool_name, item.request.risk_level);
            println!("   → Approving calculator request");

            item.response_tx.send(ApprovalDecision::approve()).unwrap();
        }
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let result = session4.execute_tool("calculator", serde_json::json!({"expression": "7*8"})).await.unwrap();
    println!("   ✓ Result: success={}, content={}", result.success, result.content);
    println!();

    approve_handler.abort();

    // ─── Summary ──────────────────────────────────────────────────────
    println!("╔══════════════════════════════════════╗");
    println!("║   Channel Demo Complete!             ║");
    println!("║   Approval Responses Tested:         ║");
    println!("║   • Approved (no modification)       ║");
    println!("║   • Denied (with reason)             ║");
    println!("║   • Modified (args changed)          ║");
    println!("║   • Auto-approved (low risk)         ║");
    println!("║   • Manual-only (all need review)    ║");
    println!("╚══════════════════════════════════════╝");
}