//! # OneAI A2A — Agent-to-Agent Protocol Rust SDK
//!
//! Implementation of the Google A2A (Agent-to-Agent) open protocol for
//! enabling inter-agent communication and collaboration. This crate provides
//! a client-side SDK for discovering remote agents and collaborating on tasks.
//!
//! ## Architecture
//!
//! The A2A protocol uses a **Task-centric** model for agent communication:
//!
//! 1. **Discovery**: Client fetches a remote agent's `AgentCard` to learn
//!    its capabilities (skills, streaming support, authentication)
//! 2. **Task initiation**: Client creates a Task by sending a Message
//! 3. **Interaction**: Agent processes the task, may request additional input
//! 4. **Completion**: Agent returns the Task result via Artifacts
//!
//! ## Core Types
//!
//! - **AgentCard**: Discovery metadata — name, skills, capabilities, auth
//! - **Task**: Central unit of work — tracks lifecycle state and history
//! - **Message**: Communication turn — role + polymorphic content parts
//! - **Part**: Content unit — Text, File (URI/base64), or Data (JSON)
//! - **Artifact**: Task output — generated files, reports, data
//! - **TaskState**: Lifecycle enum — submitted→working→completed/failed/etc.
//!
//! ## Usage
//!
//! ```ignore
//! // Create a client targeting a remote agent
//! let mut client = A2AClient::new("https://remote-agent.example.com");
//!
//! // Discover the agent's capabilities
//! let card = client.discover().await?;
//!
//! // Send a task
//! let task = client.send_task(
//!     "task-001",
//!     Message::user_text("Analyze this code"),
//!     None,
//! ).await?;
//!
//! // Check task status
//! let updated = client.get_task("task-001", None).await?;
//! ```
//!
//! ## DomainPack Integration
//!
//! AgentCards can be automatically generated from OneAI's DomainPack system:
//!
//! ```ignore
//! let domain = coding_pack("/project/dir");
//! let card = agent_card_from_domain_pack(&domain, "https://my-agent.example.com");
//! ```
//!
//! ## MCP vs A2A
//!
//! - **MCP** (Model Context Protocol): Agent ↔ Tool (vertical connection)
//! - **A2A** (Agent-to-Agent): Agent ↔ Agent (horizontal connection)
//! - Together they form the complete Agent ecosystem connectivity

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.


pub mod types;
pub mod error;
pub mod transport;
pub mod client;
pub mod card;
pub mod task_store;
pub mod handler;
pub mod router;
pub mod server;

// ─── Public exports ──────────────────────────────────────────────────────────────

// Core types
pub use types::{
    TaskState, Part, FileContent, Message, MessageRole,
    TaskStatus, Task, Artifact, AgentSkill, AgentCapabilities,
    AuthenticationInfo, AgentCard, AgentProvider,
    PushNotificationConfig,
    SendTaskParams, GetTaskParams, CancelTaskParams,
};

// Error types
pub use error::{A2AError, Result};

// Transport types
pub use transport::{
    JsonRpcRequest, JsonRpcResponse, JsonRpcError,
    TaskStreamEvent, parse_sse_event,
    METHOD_TASKS_SEND, METHOD_TASKS_SEND_SUBSCRIBE,
    METHOD_TASKS_GET, METHOD_TASKS_CANCEL, METHOD_AGENT_GET_CARD,
};

// Client
pub use client::{A2AClient, TaskStream};

// Card generation
pub use card::{
    agent_card_from_domain_pack,
    parse_agent_card,
    parse_agent_card_yaml,
    well_known_agent_card,
};

// Task store
pub use task_store::TaskStore;

// Server-side
pub use handler::A2AHandler;
pub use router::A2ARouter;
pub use server::A2AServerHost;
