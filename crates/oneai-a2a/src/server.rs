//! A2A Server Host — serves OneAI agent capabilities via A2A JSON-RPC protocol.
//!
//! The A2AServerHost makes a OneAI agent discoverable and reachable by remote
//! A2A agents. It serves:
//! - `GET /.well-known/agent-card` → AgentCard discovery endpoint
//! - `POST /` → A2A JSON-RPC protocol endpoint
//!
//! This closes the fundamental asymmetry where OneAI could discover other
//! agents (as an A2A client) but could not be discovered itself.
//!
//! ## Usage
//! ```ignore
//! let host = A2AServerHost::new(agent_card, task_store);
//! host.process_message(json_rpc_message).await;  // Process a single message
//! host.run(port).await;  // Run as HTTP server on port 8080
//! ```
//!
//! ## Architecture
//!
//! A2AServerHost follows the same pattern as McpServerHost:
//! - Holds AgentCard (identity) + TaskStore (lifecycle) + A2ARouter (dispatch)
//! - A2ARouter dispatches to A2AHandler methods
//! - A2AHandler processes each method (getCard, sendTask, getTask, cancelTask)

use std::sync::Arc;

use crate::types::AgentCard;
use crate::task_store::TaskStore;
use crate::router::A2ARouter;
use crate::handler::A2AHandler;

/// A2A server host — serves an OneAI agent's capabilities via the A2A protocol.
///
/// Makes the agent discoverable by remote A2A agents via the AgentCard
/// endpoint and allows remote agents to send tasks via the JSON-RPC endpoint.
///
/// The server implements the A2A lifecycle:
/// 1. Remote agent fetches AgentCard from `/.well-known/agent-card`
/// 2. Remote agent sends `tasks/send` → server creates Task, processes, returns result
/// 3. Remote agent can query task status via `tasks/get`
/// 4. Remote agent can cancel via `tasks/cancel`
pub struct A2AServerHost {
    /// AgentCard describing this agent's capabilities.
    agent_card: AgentCard,
    /// Task store for managing task lifecycle.
    task_store: Arc<TaskStore>,
    /// Router for dispatching methods to handlers.
    router: Arc<A2ARouter>,
}

impl A2AServerHost {
    /// Create a new A2A server host with an AgentCard and TaskStore.
    pub fn new(agent_card: AgentCard, task_store: Arc<TaskStore>) -> Self {
        let handler = Arc::new(A2AHandler::new(agent_card.clone(), task_store.clone()));
        let router = Arc::new(A2ARouter::new(handler));

        Self {
            agent_card,
            task_store,
            router,
        }
    }

    /// Create a server host from a DomainPack, auto-generating the AgentCard.
    ///
    /// Uses `agent_card_from_domain_pack()` to generate the AgentCard
    /// from the DomainPack's name, description, skills, and capabilities.
    pub fn from_domain_pack(domain: &oneai_domain::DomainPack, url: &str) -> Self {
        let agent_card = crate::card::agent_card_from_domain_pack(domain, url);
        let task_store = Arc::new(TaskStore::new());
        Self::new(agent_card, task_store)
    }

    /// Process a single JSON-RPC message and return the response.
    ///
    /// Useful for testing or custom transport implementations.
    /// Follows the same pattern as McpServerHost::process_message().
    pub async fn process_message(&self, message: serde_json::Value) -> serde_json::Value {
        self.router.dispatch(message).await
    }

    /// Get the AgentCard for this server.
    pub fn agent_card(&self) -> &AgentCard {
        &self.agent_card
    }

    /// Get the TaskStore.
    pub fn task_store(&self) -> &Arc<TaskStore> {
        &self.task_store
    }

    /// Get the well-known AgentCard as a pretty-printed JSON string.
    ///
    /// Suitable for serving at `/.well-known/agent-card`.
    pub fn well_known_card_json(&self) -> crate::error::Result<String> {
        crate::card::well_known_agent_card(&self.agent_card)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, TaskState};
    use oneai_domain::DomainPackBuilder;

    #[test]
    fn test_server_host_creation() {
        let card = AgentCard::new("test-agent", "Test", "https://test.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        assert_eq!(host.agent_card().name, "test-agent");
    }

    #[test]
    fn test_server_host_from_domain_pack() {
        let pack = DomainPackBuilder::new("coding")
            .description("A coding agent")
            .system_prompt("You are a coding assistant.")
            .build();

        let host = A2AServerHost::from_domain_pack(&pack, "https://coding.example.com");

        assert_eq!(host.agent_card().name, "coding");
        assert_eq!(host.agent_card().url, "https://coding.example.com");
    }

    #[tokio::test]
    async fn test_server_host_get_card() {
        let card = AgentCard::new("my-agent", "My agent", "https://my.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agent/getCard",
            "params": {}
        })).await;

        let result = response.get("result").unwrap();
        assert_eq!(result.get("name").and_then(|n| n.as_str()), Some("my-agent"));
    }

    #[tokio::test]
    async fn test_server_host_send_task() {
        let card = AgentCard::new("task-agent", "Task test", "https://task.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/send",
            "params": {
                "id": "task-001",
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": "Analyze code"}]
                }
            }
        })).await;

        let result = response.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-001"));
        // Should be Completed
        let status = result.get("status").unwrap();
        assert_eq!(status.get("state").and_then(|s| s.as_str()), Some("completed"));
    }

    #[tokio::test]
    async fn test_server_host_get_task() {
        let card = AgentCard::new("get-agent", "Get test", "https://get.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store.clone());

        // Create a task manually
        store.create_task("task-002", Message::user_text("Manual task")).await.unwrap();
        store.transition_task("task-002", TaskState::Working).await.unwrap();

        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tasks/get",
            "params": {
                "id": "task-002"
            }
        })).await;

        let result = response.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-002"));
        assert_eq!(result.get("status").unwrap().get("state").and_then(|s| s.as_str()), Some("working"));
    }

    #[tokio::test]
    async fn test_server_host_cancel_task() {
        let card = AgentCard::new("cancel-agent", "Cancel test", "https://cancel.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store.clone());

        // Create a task in Working state
        store.create_task("task-003", Message::user_text("Cancel me")).await.unwrap();
        store.transition_task("task-003", TaskState::Working).await.unwrap();

        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tasks/cancel",
            "params": {
                "id": "task-003"
            }
        })).await;

        let result = response.get("result").unwrap();
        let status = result.get("status").unwrap();
        assert_eq!(status.get("state").and_then(|s| s.as_str()), Some("canceled"));
    }

    #[tokio::test]
    async fn test_server_host_unknown_method() {
        let card = AgentCard::new("error-agent", "Error test", "https://error.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "unknown/method",
            "params": {}
        })).await;

        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32601));
    }

    #[tokio::test]
    async fn test_server_host_full_protocol_flow() {
        let card = AgentCard::new("full-agent", "Full protocol test", "https://full.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        // Step 1: Discover agent
        let card_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agent/getCard",
            "params": {}
        })).await;

        let card_result = card_response.get("result").unwrap();
        assert_eq!(card_result.get("name").and_then(|n| n.as_str()), Some("full-agent"));

        // Step 2: Send a task
        let send_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/send",
            "params": {
                "id": "task-flow",
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": "Execute this"}]
                }
            }
        })).await;

        let task_result = send_response.get("result").unwrap();
        assert_eq!(task_result.get("id").and_then(|v| v.as_str()), Some("task-flow"));

        // Step 3: Get the task status
        let get_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tasks/get",
            "params": {
                "id": "task-flow"
            }
        })).await;

        let get_result = get_response.get("result").unwrap();
        assert_eq!(get_result.get("id").and_then(|v| v.as_str()), Some("task-flow"));
        assert_eq!(get_result.get("status").unwrap().get("state").and_then(|s| s.as_str()), Some("completed"));
    }

    #[test]
    fn test_well_known_card_json() {
        let card = AgentCard::new("json-agent", "JSON test", "https://json.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let json = host.well_known_card_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get("name").and_then(|n| n.as_str()), Some("json-agent"));
    }
}
