//! CLI commands for session management — list, resume, delete, info.
//!
//! These commands operate on the SQLite session store to manage
//! saved conversations and enable session resume.

use oneai_core::traits::MemoryPersistence;
use oneai_persistence::SqliteSessionStore;

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

            println!("\nNote: Full session resume with agent loop requires the interactive CLI.");
            println!("Use: oneai chat --resume {}", session_id);
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
