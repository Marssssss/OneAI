//! # OneAI Supervisor — headless daemon for long-lived `AgentLoop` instances.
//!
//! OneAI's native apps (macOS / Windows / iOS / Android / HarmonyOS) lose
//! their session when backgrounded or killed. `FileWorkingStateStore`
//! persists a task's goal/steps/decisions, but not the *live* reconnect
//! handle. The supervisor closes that gap: a background daemon supervises
//! long-lived `AgentLoop` instances, persists an instance registry at
//! `~/.oneai/server/instances.json`, exposes `spawn/list/stop/status/rpc/
//! rpc_stream` over IPC (Unix domain socket on Unix, named pipe on Windows),
//! and lets a native app reconnect after a kill via
//! [`InstanceRegistry::recover_after_restart`].
//!
//! ## Layering
//!
//! This crate sits **below** `oneai-app`, exactly like `oneai-studio`. It
//! cannot hold an `App` / `AppSession` or call `run_agent` directly, so it
//! defines the [`SupervisorRunner`] + [`InstanceHandle`] traits; the CLI
//! (`examples/cli/src/cmd_supervisor`) builds a real `App` + `AppSession` per
//! spawned instance and supplies the impl. No `AppBuilder` method is added —
//! one `AppBuilder` = one `App` = one session, but the supervisor needs N
//! per-instance sessions (the studio precedent).
//!
//! ## Scope (evolution-plan §2.2)
//!
//! In-process supervised tokio tasks today; OS-process isolation is an
//! opt-in follow-up. OTEL trace reuse via `oneai-trace` (already wired).
//!
//! ## Stability
//!
//! Public enums carry `#[non_exhaustive]` per the v0.2.0+ stability commitment.

pub mod error;
pub mod protocol;
pub mod registry;
pub mod runner;
pub mod supervisor;
pub mod transport;

pub mod client;
pub mod server;

pub use client::SupervisorClient;
pub use error::{Result, SupervisorError};
pub use protocol::{decode, encode, Request, Response, RpcMethod, StreamLine};
pub use registry::{InstanceInfo, InstanceRegistry, InstanceSpec, InstanceStatus};
pub use runner::{paradigm_to_string, InstanceHandle, SupervisorRunner, TurnSummary};
pub use server::{serve, serve_with_trace, SupervisorConfig, SupervisorServer};
pub use supervisor::{Event, Supervisor};
pub use transport::{
    connect, default_server_dir, default_socket_path, mem_listener, IpcListener, IpcStream,
    MemListenerHandle,
};
