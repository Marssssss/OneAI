//! CLI commands for session management — list, resume, delete, info, and
//! HuggingFace-dataset export.
//!
//! These commands operate on the SQLite session store to manage
//! saved conversations and enable session resume.

use std::sync::OnceLock;

use oneai_core::traits::MemoryPersistence;
use oneai_core::{Message, TaskEvent};
use oneai_persistence::working_state_store::FileWorkingStateStore;
use oneai_persistence::SqliteSessionStore;
use regex::Regex;
use serde::Serialize;

/// List all saved sessions.
pub fn cmd_session_list() {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let sessions = rt.block_on(async { store.list_conversations().await });

    match sessions {
        Ok(sessions) => {
            if sessions.is_empty() {
                println!("No saved sessions found.");
                println!("Sessions are created automatically when SQLite persistence is enabled.");
                println!("Enable with: oneai chat --persist");
                return;
            }
            let total = sessions.len();
            println!("Saved sessions:");
            println!("{:<40} {:<20} {:<8} Created", "ID", "Updated", "Msgs");
            println!("{}", "-".repeat(90));
            for session in &sessions {
                println!(
                    "{:<40} {:<20} {:<8} {}",
                    session.id,
                    session.updated_at.format("%Y-%m-%d %H:%M"),
                    session.message_count,
                    session.created_at.format("%Y-%m-%d %H:%M"),
                );
            }
            println!();
            println!("Total: {} sessions", total);
        }
        Err(e) => {
            eprintln!("Error listing sessions: {}", e);
        }
    }
}

/// Resume a saved session (interactive mode with prior conversation history).
pub fn cmd_session_resume(session_id: &str) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let conversation = rt.block_on(async { store.load_conversation(session_id).await });

    match conversation {
        Ok(Some(conv)) => {
            println!("Resuming session: {}", session_id);
            println!("Conversation has {} messages.", conv.messages.len());

            // Show last few messages as context
            let recent = conv.messages.iter().rev().take(5).collect::<Vec<_>>();
            if !recent.is_empty() {
                println!("\nRecent messages:");
                for msg in recent.iter().rev() {
                    let role = match msg.role {
                        oneai_core::Role::User => "User",
                        oneai_core::Role::Assistant => "Assistant",
                        oneai_core::Role::System => "System",
                        _ => "Other",
                    };
                    let text = msg.text_content();
                    let preview = if text.len() > 100 {
                        format!("{}...", &text[..100])
                    } else {
                        text
                    };
                    println!("  [{}] {}", role, preview);
                }
            }

            println!("\nNote: Full session resume with agent loop requires the interactive TUI.");
            println!(
                "Start the TUI with `oneai chat`, then run:  /session resume {}",
                session_id
            );
        }
        Ok(None) => {
            eprintln!("Session '{}' not found.", session_id);
            eprintln!("Use 'oneai session list' to see available sessions.");
        }
        Err(e) => {
            eprintln!("Error loading session '{}': {}", session_id, e);
        }
    }
}

/// Delete a saved session and its associated STM entries.
pub fn cmd_session_delete(session_id: &str) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let result = rt.block_on(async { store.delete_conversation(session_id).await });

    match result {
        Ok(()) => {
            println!("Session '{}' deleted successfully.", session_id);
        }
        Err(e) => {
            eprintln!("Error deleting session '{}': {}", session_id, e);
        }
    }
}

/// Show detailed info about a saved session.
pub fn cmd_session_info(session_id: &str) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    // Load conversation
    let conversation = rt.block_on(async { store.load_conversation(session_id).await });

    match conversation {
        Ok(Some(conv)) => {
            println!("Session: {}", session_id);
            println!("Messages: {}", conv.messages.len());

            // Load STM entries
            let stm_entries = rt.block_on(async { store.load_stm(session_id).await });

            match stm_entries {
                Ok(entries) => {
                    println!("STM entries: {}", entries.len());
                }
                Err(e) => {
                    println!("STM entries: (error: {})", e);
                }
            }

            // Show all messages
            println!("\nConversation history:");
            for (i, msg) in conv.messages.iter().enumerate() {
                let role = match msg.role {
                    oneai_core::Role::User => "User",
                    oneai_core::Role::Assistant => "Assistant",
                    oneai_core::Role::System => "System",
                    oneai_core::Role::Tool => "Tool",
                    _ => "Other",
                };
                let text = msg.text_content();
                let preview = if text.len() > 200 {
                    format!("{}...", &text[..200])
                } else {
                    text.clone()
                };
                println!("  {}. [{}] {}", i + 1, role, preview);
            }
        }
        Ok(None) => {
            eprintln!("Session '{}' not found.", session_id);
        }
        Err(e) => {
            eprintln!("Error loading session '{}': {}", session_id, e);
        }
    }
}

/// `oneai session decay` — Phase 2.4 memory decay pass (gap P2 #16).
///
/// Builds a provider-less App with SQLite persistence + the domain pack's
/// `MemoryProfile.decay` policy, loads the user's durable fact base from
/// SQLite into the in-memory archive, runs `MemoryManager::run_decay`, and
/// the superseded state persists (store_fact upserts on conflict). No LLM
/// required — decay is a pure-function + embedding pass (embeddings are
/// preserved from SQLite; no re-embed without a service).
pub async fn cmd_session_decay(
    config: &crate::config::OneaiConfig,
    user: Option<&str>,
    domain: Option<&str>,
) {
    use oneai_app::AppBuilder;
    use oneai_domain::coding_pack;

    let domain_name = config.default_domain_pack(domain);
    let pack =
        crate::cmd_pack::get_builtin_pack(&domain_name, ".").unwrap_or_else(|| coding_pack("."));
    let uid = user.unwrap_or("default").to_string();

    let app = AppBuilder::new()
        .noop_interaction_gate()
        .default_parser()
        .generation_config(config.generation.clone())
        .domain_pack(pack)
        .sqlite_persistence()
        .user_id(uid.clone())
        .build()
        .await
        .expect("session decay App build failed");

    let mm = &app.memory_manager;
    let policy = mm.decay().await;
    let Some(policy) = policy else {
        eprintln!("No decay policy configured (no domain pack).");
        return;
    };
    if !policy.enabled {
        eprintln!(
            "Decay is disabled for the '{}' domain (facts are kept forever). \
             Enable via MemoryProfile.decay.enabled (research/assistant presets do).",
            domain_name
        );
        return;
    }

    // Load the user's durable fact base into the in-memory archive so the
    // sweep operates on persisted facts (empty session_id -> cross-session).
    let Some(p) = mm.persistence() else {
        eprintln!("No SQLite persistence wired — nothing durable to decay.");
        return;
    };
    let facts = match p.load_facts(&uid, "").await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to load facts for user '{uid}': {e}");
            std::process::exit(1);
        }
    };
    let loaded = facts.len();
    mm.archive_facts(facts).await;

    let now = chrono::Utc::now();
    let report = mm.run_decay(now).await;
    println!(
        "🧹 Memory decay pass complete (user '{}', domain '{}').\n",
        uid, domain_name
    );
    println!("  loaded {} durable fact(s) from SQLite", loaded);
    if report.core_evicted.is_empty() && report.archive_forgotten.is_empty() {
        println!("  (no facts crossed the decay threshold this pass)");
    }
    if !report.core_evicted.is_empty() {
        println!(
            "  📦 core->archive (paged, still live): {}",
            report.core_evicted.join(", ")
        );
    }
    if !report.archive_forgotten.is_empty() {
        println!(
            "  🗞️  archive forgotten (superseded, auditable via include-superseded): {}",
            report.archive_forgotten.join(", ")
        );
    }
}

// ─── export-hf: HuggingFace-dataset JSONL export (Phase 3.6) ─────────────────

/// One exported session record — a single JSON line in the output `.jsonl`.
///
/// Uses the OpenAI "messages" shape (`role` + `content: Vec<ContentBlock>`)
/// because it is the most universally accepted by HF datasets / TRL SFT
/// trainers. Tool calls, tool results, and thinking blocks are preserved
/// verbatim inside `content` — they are the training signal.
#[derive(Serialize)]
struct ExportRecord<'a> {
    session_id: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    working_state_events: Option<&'a [TaskEvent]>,
    metadata: ExportMeta,
}

#[derive(Serialize)]
struct ExportMeta {
    exported_at: String,
    message_count: usize,
    redacted: usize,
    has_working_state: bool,
}

/// `oneai session export-hf <id> -o <path>` — export a saved session to a
/// HuggingFace-dataset-compatible JSONL record (Phase 3.6, inspiration P2-5).
///
/// Stitches the live conversation with its discarded-prefix archive snapshots
/// (oldest-first, same segment order `MemoryManager::merge_full_transcript`
/// uses) so the exported transcript reflects the *complete* real history,
/// including content compressed away mid-run. Redacts high-entropy secrets
/// (API keys / bearer tokens / key-value assignments) via regex so the record
/// is safe to publish. Optionally attaches a task's raw working-state event
/// log via `--task <id>` (read straight from the `FileWorkingStateStore`
/// JSONL — the projection is not enough for a training/audit payload).
///
/// Does **not** upload anywhere — the product is a local `.jsonl` the user
/// feeds to `huggingface-cli upload` themselves (keeps credentials out of the
/// process; supply-chain / external-service discipline).
pub fn cmd_session_export_hf(
    session_id: &str,
    out: &std::path::Path,
    task: Option<&str>,
    ws_root: &std::path::Path,
    redact_ips: bool,
) {
    let store = SqliteSessionStore::with_defaults();
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    let (live, snapshots) = rt.block_on(async {
        let live = store.load_conversation(session_id).await;
        let snaps = store.load_discarded_snapshots(session_id).await;
        (live, snaps)
    });

    let live = match live {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("Session '{}' not found.", session_id);
            eprintln!("Use 'oneai session list' to see available sessions.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error loading session '{}': {}", session_id, e);
            std::process::exit(1);
        }
    };
    // Snapshots load failure is non-fatal — export the live transcript alone.
    let snapshots = snapshots.unwrap_or_default();

    // Stitch: archives oldest-first, then live (matches merge_full_transcript).
    let mut messages: Vec<Message> = Vec::new();
    for snap in snapshots.iter() {
        messages.extend(snap.messages.iter().cloned());
    }
    messages.extend(live.messages.iter().cloned());
    let message_count = messages.len();

    // Optional working-state event log.
    let working_state_events: Option<Vec<TaskEvent>> = if let Some(tid) = task {
        let ws = FileWorkingStateStore::new(ws_root);
        match rt.block_on(async { ws.read_events(tid).await }) {
            Ok(events) => Some(events),
            Err(e) => {
                eprintln!("Warning: could not read working-state events for task '{}': {} — exporting conversation only.", tid, e);
                None
            }
        }
    } else {
        None
    };
    let has_working_state = working_state_events
        .as_ref()
        .map(|e| !e.is_empty())
        .unwrap_or(false);

    let record = ExportRecord {
        session_id,
        messages: &messages,
        working_state_events: working_state_events.as_deref(),
        metadata: ExportMeta {
            exported_at: chrono::Utc::now().to_rfc3339(),
            message_count,
            redacted: 0, // filled after redaction
            has_working_state,
        },
    };

    // Serialize once, then scan the JSON line for secrets (one path covers all
    // fields, including text nested inside tool_result content blocks).
    let line = serde_json::to_string(&record).unwrap_or_else(|e| {
        eprintln!("Error serializing export record: {}", e);
        std::process::exit(1);
    });
    let (redacted_line, redacted_count) = redact_json_line(&line, redact_ips);

    // Patch the metadata.redacted count in the already-redacted string. The
    // count is small and ASCII-stable, so a targeted replace is safe.
    let final_line = if redacted_count > 0 {
        redacted_line.replacen(
            "\"redacted\":0",
            &format!("\"redacted\":{}", redacted_count),
            1,
        )
    } else {
        redacted_line
    };

    if let Err(e) = std::fs::write(out, format!("{}\n", final_line)) {
        eprintln!("Error writing export to {}: {}", out.display(), e);
        std::process::exit(1);
    }

    println!("📤 Exported session '{}' to {}", session_id, out.display());
    println!("   messages: {}", message_count);
    println!("   redacted: {} secret match(es)", redacted_count);
    println!(
        "   working-state events: {}",
        if has_working_state {
            "attached"
        } else {
            "none"
        }
    );
}

/// Redact high-entropy secrets in a serialized JSON line. Returns the redacted
/// line and a count of replacements made. Patterns are length-gated to keep
/// false positives off ordinary prose; redaction errs toward safety (a
/// over-redacted token costs a little training signal, a leaked key costs
/// everything).
///
/// `redact_ips` additionally masks private/loopback IPv4 ranges.
fn redact_json_line(line: &str, redact_ips: bool) -> (String, usize) {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            // OpenAI / Anthropic style API keys: `sk-` + 20+ word chars.
            (
                Regex::new(r"sk-[A-Za-z0-9]{20,}").unwrap(),
                "[REDACTED:api_key]",
            ),
            // AWS access key id: `AKIA` + 16 upper alnum.
            (
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                "[REDACTED:aws_key]",
            ),
            // `Bearer <long token>` in Authorization headers.
            (
                Regex::new(r"Bearer\s+[A-Za-z0-9._\-]{16,}").unwrap(),
                "[REDACTED:bearer_token]",
            ),
            // Generic key=value assignments of long high-entropy secrets.
            // `r#"..."#` so the char class can contain a literal `"`.
            (
                Regex::new(r#"(?i)(api[_-]?key|secret|token|password|authorization)["']?\s*[:=]\s*["']?[A-Za-z0-9_\-]{16,}"#).unwrap(),
                "[REDACTED:secret]",
            ),
        ]
    });

    let mut current = String::from(line);
    let mut count = 0usize;
    for (re, tag) in patterns.iter() {
        let hits = re.find_iter(&current).count();
        if hits > 0 {
            current = re.replace_all(&current, *tag).into_owned();
            count += hits;
        }
    }
    if redact_ips {
        // Private/loopback/link-local IPv4 ranges. `\b` guards against
        // matching inside a longer digit run. The `regex` crate has no
        // lookaround, so word boundaries carry that load.
        let ip = Regex::new(
            r"\b(?:10|127|169\.254|172\.(?:1[6-9]|2[0-9]|3[01])|192\.168)\.\d{1,3}\.\d{1,3}\b",
        )
        .unwrap();
        let hits = ip.find_iter(&current).count();
        if hits > 0 {
            current = ip.replace_all(&current, "[REDACTED:ip]").into_owned();
            count += hits;
        }
    }
    (current, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_openai_key() {
        let key = "sk-".to_string() + &"a".repeat(40);
        let line = format!("{{\"content\":\"my key is {}\"}}", key);
        let (out, n) = redact_json_line(&line, false);
        assert!(out.contains("[REDACTED:api_key]"));
        assert!(!out.contains(&key));
        assert_eq!(n, 1);
    }

    #[test]
    fn test_redact_aws_key() {
        let line = "AKIAIOSFODNN7EXAMPLE role arn";
        let (out, n) = redact_json_line(line, false);
        assert!(out.contains("[REDACTED:aws_key]"));
        assert_eq!(n, 1);
    }

    #[test]
    fn test_redact_bearer_header() {
        let line = "Authorization: Bearer abcdefghijklmnop1234567890";
        let (out, n) = redact_json_line(line, false);
        assert!(out.contains("[REDACTED:bearer_token]"));
        assert!(!out.contains("abcdefghijklmnop1234567890"));
        assert_eq!(n, 1);
    }

    #[test]
    fn test_redact_keyvalue_assignments() {
        let cases = [
            // Plain long secret value (not AWS-shaped) → generic pattern fires.
            ("api_key = abcdefgh1234567890longval", "[REDACTED:secret]"),
            ("\"token\":\"abcdef1234567890abcdef\"", "[REDACTED:secret]"),
            ("my-password: supersecretvalue12345", "[REDACTED:secret]"),
            (
                "Authorization=Bearer_longtokenvalue12345",
                "[REDACTED:secret]",
            ),
        ];
        for (line, tag) in cases {
            let (out, n) = redact_json_line(line, false);
            assert!(out.contains(tag), "line {:?} should contain {}", line, tag);
            assert!(n >= 1, "line {:?} should redact >=1", line);
        }

        // An AWS-shaped value in an assignment is caught by the aws_key
        // pattern (which runs before the generic one) — still redacted, just
        // tagged differently. Guards against a false "no redaction" verdict.
        let aws = "api_key = AKIAIOSFODNN7EXAMPLE123";
        let (out, n) = redact_json_line(aws, false);
        assert!(n >= 1);
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE123"));
    }

    #[test]
    fn test_redact_does_not_touch_plain_text() {
        let line = "{\"role\":\"user\",\"content\":\"what is 2+2 and why is the sky blue\"}";
        let (out, n) = redact_json_line(line, false);
        assert_eq!(out, line, "ordinary prose must be untouched");
        assert_eq!(n, 0);
    }

    #[test]
    fn test_redact_ips_flag() {
        let line = "connect to 192.168.1.5 or 10.0.0.1 but not 8.8.8.8";
        // Without the flag: public 8.8.8.8 stays, but private ones also stay.
        let (out_off, n_off) = redact_json_line(line, false);
        assert_eq!(n_off, 0);
        assert!(out_off.contains("192.168.1.5"));
        // With the flag: private ranges masked, public DNS stays.
        let (out_on, n_on) = redact_json_line(line, true);
        assert!(out_on.contains("[REDACTED:ip]"));
        assert!(!out_on.contains("192.168.1.5"));
        assert!(!out_on.contains("10.0.0.1"));
        assert!(
            out_on.contains("8.8.8.8"),
            "public IP must survive redaction"
        );
        assert!(n_on >= 2);
    }

    #[test]
    fn test_redaction_tag_does_not_cascade() {
        // After replacing an api_key, the `[REDACTED:api_key]` text contains
        // the word `api_key` — the key-value pattern must NOT then re-match it
        // (it would need a `[:=]` after the key word). This guards against an
        // infinite / cascading redaction chain.
        let key = "sk-".to_string() + &"a".repeat(30);
        let line = format!("{{\"k\":\"{}\"}}", key);
        let (out, n) = redact_json_line(&line, false);
        assert_eq!(n, 1, "exactly one redaction, no cascade");
        assert_eq!(out, format!("{{\"k\":\"[REDACTED:api_key]\"}}"));
    }

    #[test]
    fn test_export_record_serializes_messages_shape() {
        // Sanity: a record with one user message round-trips to a JSON line
        // carrying the OpenAI messages shape.
        let msg = Message::user("hello world");
        let record = ExportRecord {
            session_id: "s1",
            messages: std::slice::from_ref(&msg),
            working_state_events: None,
            metadata: ExportMeta {
                exported_at: "2026-07-31T00:00:00+00:00".into(),
                message_count: 1,
                redacted: 0,
                has_working_state: false,
            },
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"session_id\":\"s1\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"hello world\""));
        assert!(!json.contains("working_state_events"), "omitted when None");
    }
}
