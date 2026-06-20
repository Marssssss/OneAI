//! A2A protocol management commands.
//!
//! Subcommands for discovering, connecting to, and serving as A2A agents.

use std::sync::Arc;
use oneai_a2a::{A2AClient, A2AServerHost, TaskStore, AgentCard};

/// Start the A2A server (serve OneAI agent capabilities).
///
/// Creates an A2AServerHost and processes messages in a loop.
/// For P4-1, this runs a simple echo-style server that creates
/// and completes tasks with placeholder responses.
pub fn cmd_a2a_serve(domain: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        // Build the agent card
        let agent_card = if let Some(domain_name) = domain {
            match domain_name {
                "coding" => {
                    let pack = oneai_domain::coding_pack(".");
                    oneai_a2a::agent_card_from_domain_pack(&pack, "http://localhost:8080")
                }
                "research" => {
                    let pack = oneai_domain::research_pack(".");
                    oneai_a2a::agent_card_from_domain_pack(&pack, "http://localhost:8080")
                }
                _ => {
                    AgentCard::new(domain_name, format!("{} agent", domain_name), "http://localhost:8080")
                }
            }
        } else {
            AgentCard::new("oneai-agent", "OneAI Agent", "http://localhost:8080")
        };

        let task_store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(agent_card, task_store);

        println!("🤖 A2A Server starting...");
        println!("   Agent: {} ({})", host.agent_card().name, host.agent_card().url);
        println!("   Skills: {}", host.agent_card().skills.len());
        println!("   Capabilities: streaming={}, push_notifications={}, state_history={}",
            host.agent_card().capabilities.streaming,
            host.agent_card().capabilities.push_notifications,
            host.agent_card().capabilities.state_transition_history,
        );

        // Print well-known agent card
        if let Ok(card_json) = host.well_known_card_json() {
            println!("\n📋 Agent Card (/.well-known/agent-card):");
            println!("{}", card_json);
        }

        println!("\nPress Ctrl+C to stop the server.");

        // Simple event loop — for now, just keep running
        // Full HTTP server with axum will be added in a future phase
        tokio::signal::ctrl_c().await.expect("Failed to listen for ctrl+c");
        println!("Server stopped.");
    });
}

/// Discover a remote A2A agent's capabilities.
///
/// Connects to the remote agent and fetches its AgentCard.
pub fn cmd_a2a_discover(url: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let mut client = A2AClient::new(url);

        println!("🔍 Discovering A2A agent at: {}", url);

        match client.discover().await {
            Ok(card) => {
                println!("✅ Agent discovered!");
                println!("   Name: {}", card.name);
                println!("   Description: {}", card.description);
                println!("   URL: {}", card.url);
                println!("   Version: {}", card.version.unwrap_or_default());
                if let Some(provider) = &card.provider {
                    println!("   Provider: {} ({})", provider.organization, provider.url.as_deref().unwrap_or(""));
                }
                println!("   Skills:");
                for skill in &card.skills {
                    println!("     • {} [{}]: {}", skill.name, skill.id, skill.description);
                    if !skill.examples.is_empty() {
                        println!("       Examples: {}", skill.examples.join(", "));
                    }
                }
                println!("   Capabilities:");
                println!("     Streaming: {}", card.capabilities.streaming);
                println!("     Push notifications: {}", card.capabilities.push_notifications);
                println!("     State history: {}", card.capabilities.state_transition_history);
                println!("   Authentication: {} schemes", card.authentication.schemes.len());
                for scheme in &card.authentication.schemes {
                    println!("     • {}", scheme);
                }
            }
            Err(e) => {
                eprintln!("❌ Discovery failed: {}", e);
            }
        }
    });
}

/// List configured A2A endpoints.
///
/// Reads from the A2A client configuration (placeholder for future).
pub fn cmd_a2a_list() {
    println!("📋 A2A Endpoints\n");
    println!("  No configured endpoints yet.");
    println!("  Use: oneai a2a discover <url> to find remote agents");
    println!("  Use: oneai a2a serve to start as an A2A server");
}

/// Send a task to a remote A2A agent.
///
/// Creates a task with a text message and sends it to the remote agent.
pub fn cmd_a2a_send(url: &str, message: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let mut client = A2AClient::new(url);
        let task_id = format!("oneai-task-{}", uuid::Uuid::new_v4());

        println!("📤 Sending task to: {}", url);
        println!("   Message: {}", message);
        println!("   Task ID: {}", task_id);

        match client.send_task(
            &task_id,
            oneai_a2a::Message::user_text(message),
            None,
        ).await {
            Ok(task) => {
                println!("✅ Task created!");
                println!("   ID: {}", task.id);
                println!("   State: {}", task.status.state);
                if let Some(session_id) = &task.session_id {
                    println!("   Session: {}", session_id);
                }
            }
            Err(e) => {
                eprintln!("❌ Send failed: {}", e);
            }
        }
    });
}
