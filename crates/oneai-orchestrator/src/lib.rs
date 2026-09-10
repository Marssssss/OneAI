//! OneAI Orchestrator — the cloud session control plane (MVS2).
//!
//! Implements `docs/cloud-orchestrator-design.md` §6 MVS2: **one container
//! per session** (D1), running the **unmodified engine binary** (D2, G2),
//! with a thin control plane that owns session lifecycle, authentication,
//! routing and the WS reverse proxy (D3/D6/D7).
//!
//! # Planes
//!
//! - **Control plane** (`server.rs` + `routes.rs`): axum HTTP/WS server —
//!   `POST/GET/DELETE /v1/sessions*`, `GET /v1/sessions/{id}/ws` (reverse
//!   proxy), `GET /healthz`. Bearer auth via `oneai-http-auth`
//!   (`ONEAI_ORCHESTRATOR_SECRET`).
//! - **Session state** (`fsm.rs` + `registry.rs`): D6 lifecycle FSM with CAS
//!   transitions; routing table persisted as whole-file atomic JSON
//!   (supervisor-registry pattern) with startup reconcile (alive containers
//!   re-mount, dead ones mark `Crashed` for lazy resume).
//! - **Container backend** (`runner.rs` + `docker.rs`): `ContainerRunner`
//!   trait; `DockerRunner` built from pure argv builders (golden-tested like
//!   `oneai-tool/src/terminal/docker.rs`). `K8sRunner` lands in MVS4.
//! - **Proxy** (`proxy.rs`): pure WS passthrough — JSON-RPC payloads are
//!   never parsed, so engine protocol evolution doesn't touch the
//!   orchestrator.
//! - **Hibernation** (`idle.rs`): idle sweep stops sessions with no attached
//!   connections past `idle_timeout_secs`; the next request resumes them.
//!
//! # Security posture (MVS2)
//!
//! Frontend → orchestrator: shared bearer secret. Orchestrator → container:
//! container ports are published on `127.0.0.1` only (never LAN-reachable);
//! the per-session internal secret of design D7 is **deferred** until the
//! engine grows an optional ws auth hook (engine zero-change constraint).
//! TLS is terminated by a fronting reverse proxy (Caddy/ALB) per design §4.

pub mod config;
pub mod docker;
pub mod error;
pub mod fsm;
pub mod idle;
pub mod proxy;
pub mod registry;
pub mod routes;
pub mod runner;
pub mod server;

pub use config::{OrchestratorConfig, ORCHESTRATOR_SECRET_ENV};
pub use docker::DockerRunner;
pub use error::{OrchestratorError, Result};
pub use fsm::{SessionSnapshot, SessionState};
pub use registry::RoutingTable;
pub use runner::{ContainerHandle, ContainerRunner, SessionSpec};
pub use server::{run, OrchestratorState};
