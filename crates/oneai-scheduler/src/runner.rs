//! The delivery seam — the one place the cron orchestrator touches the agent.
//!
//! `oneai-scheduler` sits *below* `oneai-app` (no `oneai-*` deps beyond
//! `oneai-core`) so it cannot call `run_agent` directly. [`CronRunner`] mirrors
//! the gateway's [`GatewayRunner`]: the CLI builds a real `App` + the gateway,
//! supplies an impl that routes a fired job's task into the gateway's
//! `deliver_scheduled` (which runs a turn in the job's bound session and relays
//! the reply over the platform — `deliver=origin`, §3.2), and the orchestrator
//! core calls [`CronRunner::deliver`] per fired job.
//!
//! [`GatewayRunner`]: ../../oneai_gateway/runner/trait.GatewayRunner.html

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::job::CronJob;

/// Outcome of delivering a fired job's task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeliveryOutcome {
    /// The turn completed; `reply` is the agent's answer (relayed to the
    /// origin channel for `Origin` jobs by the runner itself).
    Done { reply: String, iterations: usize },
    /// The runner rejected the job (no provider, no bound channel, busy).
    Rejected { reason: String },
    /// Delivery failed with an error.
    Error { message: String },
}

/// The seam the CLI implements to drive a fired cron job into a real agent
/// turn. Implementations typically hold an `Arc<oneai_gateway::Gateway>` (or
/// an `Arc<dyn GatewayRunner>`) and call `deliver_scheduled(...)`.
#[async_trait]
pub trait CronRunner: Send + Sync {
    /// Deliver `job`'s task. For `Origin` jobs the impl relays the reply over
    /// the originating platform; for `Silent` jobs it runs the turn and drops
    /// the reply (logging only).
    async fn deliver(&self, job: &CronJob) -> Result<DeliveryOutcome>;
}
