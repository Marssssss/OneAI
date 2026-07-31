//! The `schedule` agent tool — lets the model create / inspect / remove /
//! manually fire cron jobs from chat, so a user can say "每天9点总结commits"
//! and the agent wires it (Phase 3.2 agent-side seam).
//!
//! Holds an `Arc<dyn CronScheduler>` (the trait seam in `oneai-core`). Only
//! registered when `AppBuilder.cron_provider(...)` is set — zero footprint
//! when no scheduler is configured (the `AgentLoop` never sees it).
//!
//! The tool stays *below* `oneai-app` (only `oneai-core` dep): it calls the
//! trait's `add_job` / `list_jobs` / `remove_job` / `trigger_job`, which the
//! concrete `CronSchedulerImpl` implements against its `JobStore`. The model
//! supplies the schedule in the existing `parse_schedule` dialect
//! (`"30m"` / `"every 2h"` / ISO / `"0 9 * * *"`) — it translates free-form NL
//! ("每天早上9点") to a cron expr itself (no separate NL parser needed; the
//! LLM is the parser).

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{CronJobSpec, CronScheduler, Tool};
use oneai_core::{RiskLevel, ToolOutput};

/// The schedule-management tool. One tool, an `action` field selects the op
/// (keeps the per-session schema small — one tool, not four).
pub struct ScheduleTool {
    scheduler: Arc<dyn CronScheduler>,
}

impl ScheduleTool {
    pub fn new(scheduler: Arc<dyn CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for ScheduleTool {
    fn name(&self) -> &str {
        "schedule"
    }

    fn description(&self) -> &str {
        "Schedule a recurring or one-shot task (cron job) that the agent runs \
         automatically on a schedule. Action 'add' creates a job: translate the \
         user's natural-language time to a schedule dialect — '30m' / 'every 2h' \
         / ISO '2026-08-01T09:00:00Z' / 5-field cron '0 9 * * *' (min hour dom \
         month dow; 0=Sunday; */N, N, N,M, A-B). 'list' shows all jobs, 'remove' \
         deletes by id, 'trigger' fires one now. Delivery 'origin' relays the \
         reply to the bound channel; 'silent' runs without replying. The task is \
         an arbitrary prompt the agent runs each fire."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove", "trigger"],
                    "description": "What to do."
                },
                "name":       { "type": "string", "description": "(add) Human-readable job name." },
                "schedule":   { "type": "string", "description": "(add) '30m' / 'every 2h' / ISO / '0 9 * * *'." },
                "task":       { "type": "string", "description": "(add) The prompt to run each fire." },
                "platform":   { "type": "string", "default": "loopback", "description": "(add) Originating platform for deliver=origin." },
                "channel":    { "type": "string", "description": "(add) Originating channel to relay the reply to." },
                "session_id": { "type": "string", "description": "(add) Session to deliver into; empty = fresh." },
                "pack":       { "type": "string", "default": "coding", "description": "(add) Bound DomainPack." },
                "deliver":    { "type": "string", "default": "origin", "enum": ["origin", "silent"] },
                "id":         { "type": "string", "description": "(remove/trigger) Job id." }
            },
            "required": ["action"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "add" => self.do_add(args).await,
            "list" => self.do_list().await,
            "remove" => self.do_remove(args).await,
            "trigger" => self.do_trigger(args).await,
            other => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!(
                    "unknown action '{other}' (add|list|remove|trigger)"
                )),
            }),
        }
    }
}

impl ScheduleTool {
    async fn do_add(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let schedule = args
            .get("schedule")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || schedule.is_empty() || task.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("add requires name, schedule, and task".to_string()),
            });
        }
        let spec = CronJobSpec {
            id: String::new(), // let the provider generate one
            name,
            schedule,
            task,
            platform: args
                .get("platform")
                .and_then(|v| v.as_str())
                .unwrap_or("loopback")
                .to_string(),
            channel: args
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            session_id: args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            pack: args
                .get("pack")
                .and_then(|v| v.as_str())
                .unwrap_or("coding")
                .to_string(),
            user_id: String::new(),
            deliver: args
                .get("deliver")
                .and_then(|v| v.as_str())
                .unwrap_or("origin")
                .to_string(),
            enabled: true,
            metadata: std::collections::HashMap::new(),
        };
        match self.scheduler.add_job(spec).await {
            Ok(id) => Ok(ToolOutput {
                success: true,
                content: format!(
                    "Scheduled job '{id}'. It will fire per its schedule; \
                     `oneai cron serve` must be running to deliver."
                ),
                error: None,
            }),
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("{e}")),
            }),
        }
    }

    async fn do_list(&self) -> Result<ToolOutput> {
        match self.scheduler.list_jobs().await {
            Ok(jobs) => {
                if jobs.is_empty() {
                    return Ok(ToolOutput {
                        success: true,
                        content: "No scheduled jobs.".to_string(),
                        error: None,
                    });
                }
                let mut out = String::new();
                for j in jobs {
                    out.push_str(&format!(
                        "- {} | {} | schedule={} | deliver={} | task={}\n",
                        j.id, j.name, j.schedule, j.deliver, j.task
                    ));
                }
                Ok(ToolOutput {
                    success: true,
                    content: out,
                    error: None,
                })
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("{e}")),
            }),
        }
    }

    async fn do_remove(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("remove requires id".to_string()),
            });
        }
        match self.scheduler.remove_job(id).await {
            Ok(true) => Ok(ToolOutput {
                success: true,
                content: format!("Removed job '{id}'."),
                error: None,
            }),
            Ok(false) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("No job '{id}'.")),
            }),
            Err(e) => Err(OneAIError::Other(format!("{e}"))),
        }
    }

    async fn do_trigger(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("trigger requires id".to_string()),
            });
        }
        match self.scheduler.trigger_job(id).await {
            Ok(true) => Ok(ToolOutput {
                success: true,
                content: format!("Fired job '{id}' (delivered via the gateway)."),
                error: None,
            }),
            Ok(false) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Could not fire '{id}' (not found / disabled).")),
            }),
            Err(e) => Err(OneAIError::Other(format!("{e}"))),
        }
    }
}
